//! Replaces a container with a new one made from the image its reference names now
//! ("Update"), keeping everything else about it: the settings it was created with,
//! its volumes (anonymous ones too) and its networks.
//!
//! [`plan`] works on the inspect JSON (the API's own field names), so the rules can be
//! tested on plain JSON; [`replace`] runs the steps against a [`Steps`] and puts the
//! original container back when one of them fails. Nothing here takes a path or a
//! setting from the UI: every value comes from the daemon's own record of the
//! container being replaced.

use std::future::Future;

use serde_json::{Map, Value, json};

use crate::EngineError;
use crate::model::ImageFreshness;

type R<T> = Result<T, EngineError>;

/// The label compose puts on the containers it manages.
const COMPOSE_PROJECT: &str = "com.docker.compose.project";

/// What [`replace`] needs to make the new container and to put the old one back.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    /// The container's name (no leading `/`), which the new one takes over.
    pub name: String,
    /// The name the old container is kept under until the new one runs.
    pub parked: String,
    /// The create body (`Config` fields, `HostConfig` and `NetworkingConfig`).
    pub body: Value,
    /// Networks to connect after creating (a container is created on one), with the
    /// endpoint settings to keep (aliases, static addresses, links).
    pub networks: Vec<(String, Value)>,
    /// Whether the old container was running, so the new one is started.
    pub running: bool,
}

/// The reference to look for a newer image under: the one the container was created
/// with. A container made from an image ID has none.
pub(crate) fn reference(container: &Value) -> R<String> {
    let reference = container["Config"]["Image"].as_str().unwrap_or_default();
    if reference.is_empty() || is_image_id(reference) {
        return Err(EngineError::Local(format!(
            "This container was created from image ID {}, not a tag, so there is no newer image to look for.",
            short(reference.trim_start_matches("sha256:"))
        )));
    }
    Ok(reference.to_owned())
}

/// Whether `reference` pins a digest (`name@sha256:…`): pulling it again changes nothing.
pub(crate) fn is_pinned(reference: &str) -> bool {
    reference.contains('@')
}

/// Builds the [`Plan`] from the container's inspect JSON, the inspect JSON of the image
/// it runs now (`old_image`, whose defaults are left for the new image to supply) and
/// the reference to create the new one from.
pub(crate) fn plan(container: &Value, old_image: Option<&Value>, reference: &str) -> R<Plan> {
    let id = container["Id"].as_str().unwrap_or_default();
    let name = container["Name"]
        .as_str()
        .unwrap_or_default()
        .trim_start_matches('/')
        .to_owned();
    if id.is_empty() || name.is_empty() {
        return Err(EngineError::Local(
            "The container's inspect data has no ID or name.".into(),
        ));
    }
    let config = container["Config"].as_object().cloned().unwrap_or_default();
    if let Some(project) = config
        .get("Labels")
        .and_then(|l| l.get(COMPOSE_PROJECT))
        .and_then(Value::as_str)
    {
        return Err(EngineError::Local(format!(
            "{name} belongs to the compose project {project}: update it with the stack's Pull and Up, which keep the project's own settings."
        )));
    }
    if container["HostConfig"]["AutoRemove"].as_bool() == Some(true) {
        // Stopping it would make the daemon delete it (and its anonymous volumes),
        // leaving nothing to put back if a later step fails.
        return Err(EngineError::Local(format!(
            "{name} was started with --rm: stopping it would delete it, so it cannot be updated in place."
        )));
    }
    let image_config = old_image.map(|i| &i["Config"]);
    let mut host = container["HostConfig"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let network_mode = host
        .get("NetworkMode")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let mut body = config;
    body.insert("Image".into(), Value::String(reference.to_owned()));
    leave_image_defaults(&mut body, image_config, &host);
    // Set by the daemon, or by the CLI from its own options: the new container gets its
    // own, or they conflict with what is kept.
    body.remove("MacAddress");
    let own_hostname = body
        .get("Hostname")
        .and_then(Value::as_str)
        .is_some_and(|h| h.is_empty() || h == short(id));
    if own_hostname || network_mode == "host" || network_mode.starts_with("container:") {
        body.remove("Hostname");
        if network_mode.starts_with("container:") {
            body.remove("Domainname");
        }
    }

    keep_anonymous_volumes(&mut host, container);
    if let Some(links) = host.get_mut("Links") {
        *links = Value::Array(
            strings(links)
                .iter()
                .filter_map(|l| create_link(l))
                .map(Value::String)
                .collect(),
        );
    }

    // A container is created on one network (its network mode) and connected to the
    // rest afterwards: the API takes one endpoint at create before version 1.44.
    let primary = match network_mode.as_str() {
        "" | "default" => "bridge".to_owned(),
        mode => mode.to_owned(),
    };
    let mut networks = Vec::new();
    let mut endpoints = Map::new();
    let attached = container["NetworkSettings"]["Networks"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let own_stack = !(primary == "host" || primary == "none" || primary.starts_with("container:"));
    if own_stack {
        for (network, endpoint) in &attached {
            let kept = endpoint_to_keep(endpoint, id, network);
            if *network == primary {
                endpoints.insert(network.clone(), kept);
            } else {
                networks.push((network.clone(), kept));
            }
        }
    }
    networks.sort_by(|a, b| a.0.cmp(&b.0));
    body.insert("HostConfig".into(), Value::Object(host));
    if !endpoints.is_empty() {
        body.insert(
            "NetworkingConfig".into(),
            json!({ "EndpointsConfig": endpoints }),
        );
    }

    let state = &container["State"];
    let running =
        state["Running"].as_bool() == Some(true) || state["Restarting"].as_bool() == Some(true);
    Ok(Plan {
        parked: format!("{name}-replaced-{}", short(id)),
        name,
        body: Value::Object(body),
        networks,
        running,
    })
}

/// Drops the settings the container only has because its image had them, so the new
/// image's own apply: its environment (`PATH`, version variables), command, labels.
fn leave_image_defaults(
    body: &mut Map<String, Value>,
    image: Option<&Value>,
    host: &Map<String, Value>,
) {
    let Some(image) = image else {
        return;
    };
    let image_env = strings(&image["Env"]);
    if let Some(env) = body.get_mut("Env") {
        let kept: Vec<Value> = strings(env)
            .into_iter()
            .filter(|e| !image_env.contains(e))
            .map(Value::String)
            .collect();
        *env = Value::Array(kept);
    }
    if let Some(Value::Object(labels)) = body.get_mut("Labels") {
        let image_labels = image["Labels"].as_object();
        labels.retain(|k, v| image_labels.and_then(|l| l.get(k)) != Some(v));
    }
    // An entrypoint set at create resets the image's command, so the command goes only
    // with an entrypoint that goes too.
    let same = |body: &Map<String, Value>, key: &str| {
        let own = body.get(key).filter(|v| !v.is_null());
        let image = image.get(key).filter(|v| !v.is_null());
        own == image
    };
    let entrypoint_is_image = same(body, "Entrypoint");
    if entrypoint_is_image {
        body.remove("Entrypoint");
        if same(body, "Cmd") {
            body.remove("Cmd");
        }
    }
    for key in [
        "WorkingDir",
        "User",
        "Healthcheck",
        "StopSignal",
        "Shell",
        "OnBuild",
    ] {
        if same(body, key) {
            body.remove(key);
        }
    }
    // Ports and volumes the image declares come with the new image (and a port it no
    // longer exposes is not published); ones that are published stay exposed.
    let published = host.get("PortBindings").and_then(Value::as_object);
    for key in ["ExposedPorts", "Volumes"] {
        let image_keys = image[key].as_object();
        if let Some(Value::Object(own)) = body.get_mut(key) {
            own.retain(|k, _| {
                !image_keys.is_some_and(|i| i.contains_key(k))
                    || (key == "ExposedPorts" && published.is_some_and(|p| p.contains_key(k)))
            });
        }
    }
}

/// Mounts the container's anonymous volumes by name, so the new container finds the
/// data the old one wrote there instead of starting with empty volumes.
fn keep_anonymous_volumes(host: &mut Map<String, Value>, container: &Value) {
    // (target, volume name, read-only) of each anonymous volume.
    let anonymous: Vec<(&str, &str, bool)> = container["Mounts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["Type"].as_str() == Some("volume"))
        .filter_map(|m| {
            let read_only = m["RW"].as_bool() == Some(false);
            Some((m["Destination"].as_str()?, m["Name"].as_str()?, read_only))
        })
        .filter(|(target, name, _)| !target.is_empty() && is_anonymous(name))
        .collect();
    let mut mounts: Vec<Value> = host
        .get("Mounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // A volume mount given without a source (`--mount type=volume,dst=/x`) got an
    // anonymous volume: name it, or the new container would get an empty one.
    for m in &mut mounts {
        let unnamed =
            m["Type"].as_str() == Some("volume") && m["Source"].as_str().is_none_or(str::is_empty);
        let target = m["Target"].as_str().unwrap_or_default().to_owned();
        if unnamed && let Some((_, name, _)) = anonymous.iter().find(|(t, _, _)| *t == target) {
            m["Source"] = Value::String((*name).to_owned());
        }
    }
    let taken: Vec<String> = mounts
        .iter()
        .filter_map(|m| m["Target"].as_str().map(str::to_owned))
        .chain(
            host.get("Binds")
                .map(strings)
                .unwrap_or_default()
                .iter()
                .filter_map(|b| bind_target(b)),
        )
        .collect();
    for (target, name, read_only) in anonymous {
        if taken.iter().any(|t| t == target) {
            continue;
        }
        mounts.push(json!({
            "Type": "volume",
            "Source": name,
            "Target": target,
            "ReadOnly": read_only,
        }));
    }
    if !mounts.is_empty() {
        host.insert("Mounts".into(), Value::Array(mounts));
    }
}

/// The settings of an attached network's endpoint that were chosen rather than
/// assigned: aliases (less the old container's own ID, which the daemon adds), a
/// static address, links and driver options.
fn endpoint_to_keep(endpoint: &Value, id: &str, network: &str) -> Value {
    let mut kept = Map::new();
    // The default bridge takes no aliases.
    if network != "bridge" {
        let aliases: Vec<Value> = strings(&endpoint["Aliases"])
            .into_iter()
            .filter(|a| a != short(id) && a != id)
            .map(Value::String)
            .collect();
        if !aliases.is_empty() {
            kept.insert("Aliases".into(), Value::Array(aliases));
        }
    }
    for key in ["IPAMConfig", "DriverOpts"] {
        if let Some(v) = endpoint.get(key).filter(|v| is_set(v)) {
            kept.insert(key.into(), v.clone());
        }
    }
    let links: Vec<Value> = strings(&endpoint["Links"])
        .iter()
        .filter_map(|l| create_link(l))
        .map(Value::String)
        .collect();
    if !links.is_empty() {
        kept.insert("Links".into(), Value::Array(links));
    }
    Value::Object(kept)
}

/// Inspect shows a link as `/db:/web/database`; create takes `db:database`.
fn create_link(link: &str) -> Option<String> {
    let (target, alias) = link.split_once(':')?;
    let target = target.trim_start_matches('/');
    let alias = alias.rsplit('/').next().unwrap_or(alias);
    (!target.is_empty() && !alias.is_empty()).then(|| format!("{target}:{alias}"))
}

/// `src:/target[:opts]` → `/target`.
fn bind_target(bind: &str) -> Option<String> {
    let mut parts = bind.split(':');
    let _source = parts.next()?;
    parts.next().map(str::to_owned)
}

fn is_set(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Object(m) => m.values().any(is_set),
        Value::Array(a) => !a.is_empty(),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_owned))
        .collect()
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

/// Docker names an anonymous volume with 64 hex digits.
fn is_anonymous(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `sha256:<hex>` or bare hex: an image ID rather than a name.
fn is_image_id(reference: &str) -> bool {
    let hex = reference.strip_prefix("sha256:").unwrap_or(reference);
    reference.starts_with("sha256:")
        || (hex.len() >= 12
            && hex
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
}

/// Whether container image `running` is the newest for its tag: `tag` is what the tag
/// names locally (image ID and repo digests), when it is here, and `remote` the digest
/// the registry has for it now (or why it could not be read).
pub(crate) fn freshness(
    running: &str,
    tag: Option<(&str, &[String])>,
    remote: Result<&str, String>,
) -> ImageFreshness {
    match (tag, remote) {
        // Pulled since the container was made: Update only has to replace it.
        (Some((id, _)), _) if id != running => ImageFreshness::Pulled,
        (Some((_, digests)), Ok(digest)) => {
            let here = digests
                .iter()
                .any(|d| d.rsplit_once('@').is_some_and(|(_, d)| d == digest));
            if here {
                ImageFreshness::Current
            } else {
                ImageFreshness::Available
            }
        }
        // The tag is gone locally but the registry has it.
        (None, Ok(_)) => ImageFreshness::Available,
        (_, Err(e)) => ImageFreshness::Unknown(e),
    }
}

/// The daemon calls [`replace`] makes, so the order and the rollback can be tested
/// without one.
pub(crate) trait Steps {
    fn stop(&self, id: &str) -> impl Future<Output = R<()>> + Send;
    fn start(&self, id: &str) -> impl Future<Output = R<()>> + Send;
    fn rename(&self, id: &str, name: &str) -> impl Future<Output = R<()>> + Send;
    /// Creates a container named `name`; returns its ID.
    fn create(&self, name: &str, body: &Value) -> impl Future<Output = R<String>> + Send;
    fn connect(
        &self,
        network: &str,
        id: &str,
        endpoint: &Value,
    ) -> impl Future<Output = R<()>> + Send;
    /// Removes a container, never its volumes (the new container uses them).
    fn remove(&self, id: &str, force: bool) -> impl Future<Output = R<()>> + Send;
}

/// What [`replace`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Replaced {
    pub id: String,
    /// The old container's name when it could not be removed afterwards.
    pub leftover: Option<String>,
}

/// Stops the old container and parks it under another name, creates the new one
/// under the old name, connects its networks and starts it (when the old one ran),
/// then removes the old one. When a step before the removal fails, the new container
/// is removed and the old one gets its name back and is started again, so a failed
/// update leaves things as they were.
pub(crate) async fn replace(d: &impl Steps, old: &str, plan: &Plan) -> R<Replaced> {
    if plan.running {
        d.stop(old).await?;
    }
    if let Err(e) = d.rename(old, &plan.parked).await {
        return Err(restore(d, old, None, plan, e).await);
    }
    let new = match d.create(&plan.name, &plan.body).await {
        Ok(id) => id,
        Err(e) => return Err(restore(d, old, Some(&plan.name), plan, e).await),
    };
    let mut steps = Ok(());
    for (network, endpoint) in &plan.networks {
        steps = d.connect(network, &new, endpoint).await;
        if steps.is_err() {
            break;
        }
    }
    if steps.is_ok() && plan.running {
        steps = d.start(&new).await;
    }
    if let Err(e) = steps {
        if let Err(rm) = d.remove(&new, true).await {
            tracing::warn!(container = %new, error = %rm, "could not remove the new container of a failed update");
        }
        return Err(restore(d, old, Some(&plan.name), plan, e).await);
    }
    let leftover = match d.remove(old, false).await {
        Ok(()) => None,
        Err(e) => {
            tracing::warn!(container = %old, error = %e, "could not remove the replaced container");
            Some(plan.parked.clone())
        }
    };
    Ok(Replaced { id: new, leftover })
}

/// Puts the old container back after `failed`: its name (when it was parked) and, when
/// it ran, its process. Returns `failed`, or [`EngineError::NotRestored`] saying where
/// the original is when it could not be put back.
async fn restore(
    d: &impl Steps,
    old: &str,
    renamed: Option<&str>,
    plan: &Plan,
    failed: EngineError,
) -> EngineError {
    let mut kept_as = None;
    if let Some(name) = renamed
        && let Err(e) = d.rename(old, name).await
    {
        tracing::warn!(container = %old, error = %e, "could not give the old container its name back");
        kept_as = Some(plan.parked.clone());
    }
    let stopped = plan.running
        && match d.start(old).await {
            Ok(()) => false,
            Err(e) => {
                tracing::warn!(container = %old, error = %e, "could not start the old container again");
                true
            }
        };
    if kept_as.is_none() && !stopped {
        return failed;
    }
    EngineError::NotRestored {
        error: Box::new(failed),
        kept_as: kept_as.unwrap_or_else(|| plan.name.clone()),
        stopped,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn anon() -> String {
        "b".repeat(64)
    }

    fn image() -> Value {
        json!({
            "Id": "sha256:old",
            "Config": {
                "Env": ["PATH=/usr/bin", "APP_VERSION=1.0"],
                "Cmd": ["serve"],
                "Entrypoint": ["/entry.sh"],
                "WorkingDir": "/app",
                "ExposedPorts": {"80/tcp": {}, "9000/tcp": {}},
                "Volumes": {"/cache": {}},
                "Labels": {"org.opencontainers.image.version": "1.0"}
            }
        })
    }

    fn container() -> Value {
        json!({
            "Id": ID,
            "Name": "/web",
            "Image": "sha256:old",
            "State": {"Running": true, "Restarting": false},
            "Config": {
                "Hostname": "0123456789ab",
                "Image": "example/web:1",
                "Env": ["PATH=/usr/bin", "APP_VERSION=1.0", "MODE=prod"],
                "Cmd": ["serve"],
                "Entrypoint": ["/entry.sh"],
                "WorkingDir": "/app",
                "ExposedPorts": {"80/tcp": {}, "9000/tcp": {}, "8443/tcp": {}},
                "Volumes": {"/cache": {}, "/scratch": {}},
                "Labels": {"org.opencontainers.image.version": "1.0", "team": "web"},
                "MacAddress": "02:42:ac:11:00:02"
            },
            "HostConfig": {
                "Binds": ["/srv/web:/srv:ro"],
                "PortBindings": {"80/tcp": [{"HostIp": "", "HostPort": "8080"}]},
                "RestartPolicy": {"Name": "unless-stopped", "MaximumRetryCount": 0},
                "NetworkMode": "front",
                "Links": ["/db:/web/database"]
            },
            "Mounts": [
                {"Type": "bind", "Source": "/srv/web", "Destination": "/srv", "RW": false},
                {"Type": "volume", "Name": anon(), "Destination": "/cache", "RW": true},
                {"Type": "volume", "Name": "data", "Destination": "/data", "RW": true}
            ],
            "NetworkSettings": {"Networks": {
                "front": {
                    "Aliases": ["web", "0123456789ab"],
                    "IPAMConfig": {"IPv4Address": "172.30.0.10"},
                    "IPAddress": "172.30.0.10",
                    "NetworkID": "n1",
                    "Links": null
                },
                "back": {"Aliases": null, "IPAMConfig": null, "NetworkID": "n2"}
            }}
        })
    }

    #[test]
    fn the_new_container_keeps_its_settings_but_takes_the_new_images_defaults() {
        let p = plan(&container(), Some(&image()), "example/web:1").unwrap();
        assert_eq!(p.name, "web");
        assert_eq!(p.parked, "web-replaced-0123456789ab");
        assert!(p.running);
        let b = &p.body;
        assert_eq!(b["Image"], "example/web:1");
        // The image's own variables come from the new image; the container's own stay.
        assert_eq!(b["Env"], json!(["MODE=prod"]));
        assert_eq!(b["Labels"], json!({"team": "web"}));
        for key in ["Cmd", "Entrypoint", "WorkingDir", "Hostname", "MacAddress"] {
            assert!(b.get(key).is_none(), "{key}: {b}");
        }
        // 80 is published, 8443 is the container's own: both stay; 9000 came with the image.
        assert_eq!(b["ExposedPorts"], json!({"80/tcp": {}, "8443/tcp": {}}));
        assert_eq!(b["Volumes"], json!({"/scratch": {}}));
        let host = &b["HostConfig"];
        assert_eq!(host["Binds"], json!(["/srv/web:/srv:ro"]));
        assert_eq!(host["RestartPolicy"]["Name"], "unless-stopped");
        assert_eq!(host["Links"], json!(["db:database"]));
        // The anonymous volume is mounted by name, so its data carries over.
        assert_eq!(
            host["Mounts"],
            json!([{"Type": "volume", "Source": anon(), "Target": "/cache", "ReadOnly": false}])
        );
        assert_eq!(
            b["NetworkingConfig"]["EndpointsConfig"],
            json!({"front": {"Aliases": ["web"], "IPAMConfig": {"IPv4Address": "172.30.0.10"}}})
        );
        assert_eq!(p.networks, vec![("back".to_owned(), json!({}))]);
    }

    #[test]
    fn a_custom_entrypoint_keeps_its_command() {
        let mut c = container();
        c["Config"]["Entrypoint"] = json!(["/bin/sh", "-c"]);
        let p = plan(&c, Some(&image()), "example/web:1").unwrap();
        assert_eq!(p.body["Entrypoint"], json!(["/bin/sh", "-c"]));
        assert_eq!(p.body["Cmd"], json!(["serve"]));
    }

    #[test]
    fn without_the_old_image_every_setting_is_kept() {
        let p = plan(&container(), None, "example/web:1").unwrap();
        assert_eq!(
            p.body["Env"],
            json!(["PATH=/usr/bin", "APP_VERSION=1.0", "MODE=prod"])
        );
        assert_eq!(p.body["Cmd"], json!(["serve"]));
    }

    #[test]
    fn a_chosen_hostname_stays_and_shared_network_stacks_have_none() {
        let mut c = container();
        c["Config"]["Hostname"] = json!("api.local");
        let p = plan(&c, None, "x:1").unwrap();
        assert_eq!(p.body["Hostname"], "api.local");

        c["HostConfig"]["NetworkMode"] = json!("container:abc");
        c["Config"]["Domainname"] = json!("local");
        let p = plan(&c, None, "x:1").unwrap();
        assert!(p.body.get("Hostname").is_none());
        assert!(p.body.get("Domainname").is_none());
        assert!(p.body.get("NetworkingConfig").is_none());
        assert!(p.networks.is_empty());

        c["HostConfig"]["NetworkMode"] = json!("host");
        let p = plan(&c, None, "x:1").unwrap();
        assert!(p.body.get("NetworkingConfig").is_none());
        assert!(p.networks.is_empty());
    }

    #[test]
    fn the_default_bridge_takes_no_aliases() {
        let mut c = container();
        c["HostConfig"]["NetworkMode"] = json!("default");
        c["NetworkSettings"]["Networks"] = json!({"bridge": {"Aliases": ["web"]}});
        let p = plan(&c, None, "x:1").unwrap();
        assert_eq!(
            p.body["NetworkingConfig"]["EndpointsConfig"],
            json!({"bridge": {}})
        );
    }

    #[test]
    fn a_volume_mount_without_a_source_keeps_its_anonymous_volume() {
        let mut c = container();
        c["HostConfig"]["Mounts"] = json!([{"Type": "volume", "Target": "/cache"}]);
        let p = plan(&c, None, "x:1").unwrap();
        // Named, not mounted twice.
        assert_eq!(
            p.body["HostConfig"]["Mounts"],
            json!([{"Type": "volume", "Target": "/cache", "Source": anon()}])
        );
    }

    #[test]
    fn a_container_started_with_rm_is_refused_before_anything_changes() {
        let mut c = container();
        c["HostConfig"]["AutoRemove"] = json!(true);
        let e = plan(&c, None, "x:1").unwrap_err();
        assert!(e.to_string().contains("--rm"), "{e}");
    }

    #[test]
    fn compose_containers_and_bad_inspect_data_are_refused() {
        let mut c = container();
        c["Config"]["Labels"][COMPOSE_PROJECT] = json!("shop");
        let e = plan(&c, None, "x:1").unwrap_err();
        assert!(e.to_string().contains("shop"), "{e}");
        let e = plan(&json!({"Id": ID}), None, "x:1").unwrap_err();
        assert!(matches!(e, EngineError::Local(_)));
    }

    #[test]
    fn a_stopped_container_is_not_started() {
        let mut c = container();
        c["State"] = json!({"Running": false, "Restarting": false});
        assert!(!plan(&c, None, "x:1").unwrap().running);
        c["State"]["Restarting"] = json!(true);
        assert!(plan(&c, None, "x:1").unwrap().running);
    }

    #[test]
    fn references_name_a_tag_or_digest_never_an_id() {
        let with = |image: &str| reference(&json!({"Config": {"Image": image}}));
        assert_eq!(with("nginx").unwrap(), "nginx");
        assert_eq!(
            with("localhost:5000/app:2").unwrap(),
            "localhost:5000/app:2"
        );
        assert!(with("sha256:0123456789abcdef").is_err());
        assert!(with("0123456789abcdef").is_err());
        assert!(with("").is_err());
        assert!(is_pinned("nginx@sha256:abc"));
        assert!(!is_pinned("nginx:1.27"));
    }

    #[test]
    fn links_and_bind_targets_parse() {
        assert_eq!(
            create_link("/db:/web/database").as_deref(),
            Some("db:database")
        );
        assert_eq!(create_link("db:database").as_deref(), Some("db:database"));
        assert_eq!(create_link("nonsense"), None);
        assert_eq!(bind_target("/a:/b:ro").as_deref(), Some("/b"));
        assert_eq!(bind_target("vol:/b").as_deref(), Some("/b"));
        assert_eq!(bind_target("/only"), None);
    }

    #[test]
    fn freshness_compares_the_local_tag_then_the_registry() {
        let digests = vec!["nginx@sha256:aaa".to_owned()];
        let here = Some(("sha256:run", digests.as_slice()));
        assert_eq!(
            freshness("sha256:run", here, Ok("sha256:aaa")),
            ImageFreshness::Current
        );
        assert_eq!(
            freshness("sha256:run", here, Ok("sha256:bbb")),
            ImageFreshness::Available
        );
        // The tag already names a newer local image: no registry needed.
        let pulled = Some(("sha256:newer", digests.as_slice()));
        assert_eq!(
            freshness("sha256:run", pulled, Err("offline".into())),
            ImageFreshness::Pulled
        );
        assert_eq!(
            freshness("sha256:run", here, Err("denied".into())),
            ImageFreshness::Unknown("denied".into())
        );
        assert_eq!(
            freshness("sha256:run", None, Ok("sha256:aaa")),
            ImageFreshness::Available
        );
        // A locally built image has no repo digest to match.
        assert_eq!(
            freshness("sha256:run", Some(("sha256:run", &[])), Ok("sha256:aaa")),
            ImageFreshness::Available
        );
    }

    /// Records each call; fails the ones named in `fail`.
    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
        fail: Vec<&'static str>,
        fail_exactly: Vec<&'static str>,
    }

    impl Fake {
        fn failing(fail: &[&'static str]) -> Self {
            Self {
                fail: fail.to_vec(),
                ..Default::default()
            }
        }
        fn step(&self, call: String) -> R<()> {
            let failed = self.fail.iter().any(|f| call.starts_with(f))
                || self.fail_exactly.contains(&call.as_str());
            self.calls.lock().unwrap().push(call);
            if failed {
                Err(EngineError::Daemon {
                    status: 500,
                    message: "boom".into(),
                })
            } else {
                Ok(())
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Steps for Fake {
        async fn stop(&self, id: &str) -> R<()> {
            self.step(format!("stop {id}"))
        }
        async fn start(&self, id: &str) -> R<()> {
            self.step(format!("start {id}"))
        }
        async fn rename(&self, id: &str, name: &str) -> R<()> {
            self.step(format!("rename {id} {name}"))
        }
        async fn create(&self, name: &str, _body: &Value) -> R<String> {
            self.step(format!("create {name}"))
                .map(|()| "new".to_owned())
        }
        async fn connect(&self, network: &str, id: &str, _endpoint: &Value) -> R<()> {
            self.step(format!("connect {network} {id}"))
        }
        async fn remove(&self, id: &str, force: bool) -> R<()> {
            self.step(format!("remove {id} {force}"))
        }
    }

    fn test_plan(running: bool) -> Plan {
        Plan {
            name: "web".into(),
            parked: "web-replaced".into(),
            body: json!({}),
            networks: vec![("back".into(), json!({}))],
            running,
        }
    }

    fn run(d: &Fake, running: bool) -> R<Replaced> {
        futures::executor::block_on(replace(d, "old", &test_plan(running)))
    }

    #[test]
    fn replaces_in_order_and_removes_the_old_container_last() {
        let d = Fake::default();
        let r = run(&d, true).unwrap();
        assert_eq!(
            r,
            Replaced {
                id: "new".into(),
                leftover: None
            }
        );
        assert_eq!(
            d.calls(),
            [
                "stop old",
                "rename old web-replaced",
                "create web",
                "connect back new",
                "start new",
                "remove old false",
            ]
        );
    }

    #[test]
    fn a_stopped_container_is_replaced_without_starting_either() {
        let d = Fake::default();
        run(&d, false).unwrap();
        assert!(
            !d.calls().iter().any(|c| c.starts_with("st")),
            "{:?}",
            d.calls()
        );
    }

    #[test]
    fn a_failed_create_gives_the_old_container_back_its_name_and_process() {
        let d = Fake::failing(&["create"]);
        assert!(run(&d, true).is_err());
        assert_eq!(
            d.calls(),
            [
                "stop old",
                "rename old web-replaced",
                "create web",
                "rename old web",
                "start old",
            ]
        );
    }

    #[test]
    fn a_failed_start_removes_the_new_container_before_restoring() {
        let d = Fake::failing(&["start new"]);
        let e = run(&d, true).unwrap_err();
        assert!(e.to_string().contains("boom"), "{e}");
        assert_eq!(
            d.calls()[4..],
            [
                "start new",
                "remove new true",
                "rename old web",
                "start old"
            ]
        );
    }

    #[test]
    fn a_failed_network_connect_rolls_back_without_starting_the_new_one() {
        let d = Fake::failing(&["connect"]);
        assert!(run(&d, true).is_err());
        let calls = d.calls();
        assert!(!calls.contains(&"start new".to_owned()), "{calls:?}");
        assert_eq!(calls.last().map(String::as_str), Some("start old"));
    }

    #[test]
    fn an_original_that_cannot_get_its_name_back_is_reported_where_it_is() {
        let d = Fake {
            fail_exactly: vec!["create web", "rename old web"],
            ..Default::default()
        };
        let e = run(&d, true).unwrap_err();
        assert!(
            matches!(
                &e,
                EngineError::NotRestored { error, kept_as, stopped: false }
                    if kept_as == "web-replaced" && error.to_string().contains("boom")
            ),
            "{e:?}"
        );
    }

    #[test]
    fn an_original_that_will_not_start_again_is_reported_stopped() {
        let d = Fake::failing(&["start"]);
        let e = run(&d, true).unwrap_err();
        assert!(
            matches!(&e, EngineError::NotRestored { kept_as, stopped: true, .. } if kept_as == "web"),
            "{e:?}"
        );
        assert!(!e.is_connection_lost());
    }

    #[test]
    fn a_failed_stop_or_rename_changes_nothing_else() {
        let d = Fake::failing(&["stop"]);
        assert!(run(&d, true).is_err());
        assert_eq!(d.calls(), ["stop old"]);

        let d = Fake::failing(&["rename"]);
        assert!(run(&d, true).is_err());
        assert_eq!(
            d.calls(),
            ["stop old", "rename old web-replaced", "start old"]
        );
    }

    #[test]
    fn an_old_container_that_will_not_go_is_reported_not_rolled_back() {
        let d = Fake::failing(&["remove old"]);
        let r = run(&d, true).unwrap();
        assert_eq!(r.leftover.as_deref(), Some("web-replaced"));
        assert_eq!(r.id, "new");
    }
}
