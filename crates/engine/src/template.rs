//! What it takes to run another container like an existing one: the settings of
//! `docker inspect` that a [`RunSpec`] can carry, and a list of the ones it cannot.
//!
//! Works on the inspect JSON (the API's own field names), so no bollard type is
//! involved and the rules can be tested on plain JSON.

use serde_json::Value;

use crate::model::{
    ContainerTemplate, PublishedPort, RestartPolicy, RunSpec, SkippedSetting, VolumeMount,
};

/// Builds the template from a container's inspect JSON and, when known, its image's
/// (whose `Config.Env` and `Config.Cmd` are left out: the image supplies them anyway).
pub(crate) fn template_from_inspect(container: &Value, image: Option<&Value>) -> ContainerTemplate {
    let config = &container["Config"];
    let host = &container["HostConfig"];
    let image_config = image.map(|i| &i["Config"]);
    let mut skipped = Vec::new();

    let name = container["Name"]
        .as_str()
        .map(|n| n.trim_start_matches('/').to_owned())
        .filter(|n| !n.is_empty());

    let image_env = image_config.map(|c| strings(&c["Env"])).unwrap_or_default();
    let env = strings(&config["Env"])
        .into_iter()
        .filter(|e| !image_env.contains(e))
        .collect();

    let cmd = strings(&config["Cmd"]);
    let cmd = match image_config {
        Some(c) if strings(&c["Cmd"]) == cmd => Vec::new(),
        _ => cmd,
    };

    let entrypoint = strings(&config["Entrypoint"]);
    let image_entrypoint = image_config
        .map(|c| strings(&c["Entrypoint"]))
        .unwrap_or_default();
    if image_config.is_some() && entrypoint != image_entrypoint {
        skipped.push(SkippedSetting::Entrypoint(entrypoint));
    }

    let ports = ports(&host["PortBindings"]);

    let mut volumes = Vec::new();
    for m in container["Mounts"].as_array().into_iter().flatten() {
        let target = m["Destination"].as_str().unwrap_or_default().to_owned();
        let kind = m["Type"].as_str().unwrap_or_default();
        let volume = m["Name"].as_str().unwrap_or_default();
        if kind == "volume" && !volume.is_empty() && !is_anonymous(volume) {
            volumes.push(VolumeMount {
                volume: volume.to_owned(),
                target,
                read_only: m["RW"].as_bool() == Some(false),
            });
        } else {
            let source = if kind == "volume" {
                None
            } else {
                m["Source"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
            };
            skipped.push(SkippedSetting::Mount {
                kind: if kind.is_empty() { "mount" } else { kind }.to_owned(),
                source,
                target,
            });
        }
    }
    volumes.sort_by(|a, b| a.target.cmp(&b.target));

    let network = match host["NetworkMode"].as_str().unwrap_or_default() {
        "" | "default" | "bridge" => None,
        mode if mode.contains(':') => {
            // `container:<id>` shares another container's network stack.
            skipped.push(SkippedSetting::NetworkMode(mode.to_owned()));
            None
        }
        mode => Some(mode.to_owned()),
    };

    skipped.extend(
        other_settings(container, image_config)
            .into_iter()
            .map(SkippedSetting::Option),
    );

    let restart = match host["RestartPolicy"]["Name"].as_str().unwrap_or_default() {
        "always" => RestartPolicy::Always,
        "unless-stopped" => RestartPolicy::UnlessStopped,
        "on-failure" => RestartPolicy::OnFailure,
        _ => RestartPolicy::No,
    };

    ContainerTemplate {
        spec: RunSpec {
            image: config["Image"].as_str().unwrap_or_default().to_owned(),
            name,
            cmd,
            env,
            ports,
            volumes,
            network,
            restart,
        },
        skipped,
    }
}

/// The reference to run a copy with: the one the container was created with while it
/// still names the same image (`tag_id`, what it resolves to now), else the image's
/// id, which cannot have moved.
pub(crate) fn image_to_run(
    reference: &str,
    tag_id: Option<&str>,
    image_id: Option<&str>,
) -> String {
    match image_id {
        Some(id) if !id.is_empty() && tag_id != Some(id) => id.to_owned(),
        _ => reference.to_owned(),
    }
}

/// The settings no Run form field carries, as `docker run` flags, in a fixed order.
/// Values the daemon fills in by itself (no limit, the default shared memory, the
/// image's own user or labels) are not settings and are left out.
fn other_settings(container: &Value, image: Option<&Value>) -> Vec<String> {
    let config = &container["Config"];
    let host = &container["HostConfig"];
    let id = container["Id"].as_str().unwrap_or_default();
    let mut out = Vec::new();
    let mut flag = |name: &str, value: Option<String>| {
        out.push(match value {
            Some(v) => format!("{name} {v}"),
            None => name.to_owned(),
        })
    };
    let positive = |v: &Value| v.as_i64().filter(|n| *n > 0);
    let on = |v: &Value| v.as_bool() == Some(true);
    let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    let differs = |key: &str| {
        let own = text(&config[key]);
        let image = image.and_then(|i| text(&i[key]));
        own.filter(|v| Some(v) != image.as_ref())
    };

    if on(&config["Tty"]) {
        flag("-t", None);
    }
    if on(&config["OpenStdin"]) {
        flag("-i", None);
    }
    if let Some(user) = differs("User") {
        flag("--user", Some(user));
    }
    if let Some(dir) = differs("WorkingDir") {
        flag("--workdir", Some(dir));
    }
    let network_mode = host["NetworkMode"].as_str().unwrap_or_default();
    // With the host's network (or another container's) the hostname is not chosen.
    let own_network = !(network_mode == "host" || network_mode.starts_with("container:"));
    if let Some(name) = text(&config["Hostname"]).filter(|h| own_network && h != short(id)) {
        flag("--hostname", Some(name));
    }
    if let Some(domain) = text(&config["Domainname"]) {
        flag("--domainname", Some(domain));
    }
    let image_labels = image.and_then(|i| i["Labels"].as_object());
    let mut labels: Vec<String> = config["Labels"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, v)| image_labels.and_then(|l| l.get(*k)) != Some(*v))
        // compose's and Docker Desktop's own bookkeeping.
        .filter(|(k, _)| {
            !k.starts_with("com.docker.compose.") && !k.starts_with("desktop.docker.io/")
        })
        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
        .collect();
    labels.sort();
    for label in labels {
        flag("--label", Some(label));
    }
    let published = host["PortBindings"].as_object();
    let image_ports = image.and_then(|i| i["ExposedPorts"].as_object());
    for port in config["ExposedPorts"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, _)| k)
    {
        if !published.is_some_and(|p| p.contains_key(port))
            && !image_ports.is_some_and(|p| p.contains_key(port))
        {
            flag("--expose", Some(port.clone()));
        }
    }
    if let Some(signal) = differs("StopSignal") {
        flag("--stop-signal", Some(signal));
    }
    if let Some(t) = config["StopTimeout"].as_i64() {
        flag("--stop-timeout", Some(t.to_string()));
    }
    let health = &config["Healthcheck"];
    let image_health = image.map(|i| &i["Healthcheck"]).unwrap_or(&Value::Null);
    if !health.is_null() && health != image_health {
        if strings(&health["Test"]).first().map(String::as_str) == Some("NONE") {
            flag("--no-healthcheck", None);
        } else {
            flag("--health-cmd", Some(strings(&health["Test"]).join(" ")));
        }
    }

    if let Some(n) = positive(&host["RestartPolicy"]["MaximumRetryCount"]) {
        flag("--restart", Some(format!("on-failure:{n}")));
    }
    if let Some(bytes) = positive(&host["Memory"]) {
        flag("--memory", Some(size(bytes)));
        let swap = host["MemorySwap"].as_i64().unwrap_or(0);
        // The daemon sets twice the memory when no swap limit is given.
        if swap != 0 && swap != 2 * bytes {
            flag(
                "--memory-swap",
                Some(if swap < 0 { "-1".into() } else { size(swap) }),
            );
        }
    }
    if let Some(bytes) = positive(&host["MemoryReservation"]) {
        flag("--memory-reservation", Some(size(bytes)));
    }
    if let Some(nano) = positive(&host["NanoCpus"]) {
        flag("--cpus", Some(format!("{}", nano as f64 / 1e9)));
    }
    if let Some(n) = positive(&host["CpuShares"]) {
        flag("--cpu-shares", Some(n.to_string()));
    }
    if let Some(n) = positive(&host["CpuQuota"]) {
        flag("--cpu-quota", Some(n.to_string()));
    }
    if let Some(cpus) = text(&host["CpusetCpus"]) {
        flag("--cpuset-cpus", Some(cpus));
    }
    if let Some(n) = positive(&host["PidsLimit"]) {
        flag("--pids-limit", Some(n.to_string()));
    }
    // 64 MiB is the daemon's own default.
    if let Some(bytes) = positive(&host["ShmSize"]).filter(|b| *b != 64 << 20) {
        flag("--shm-size", Some(size(bytes)));
    }
    if on(&host["Privileged"]) {
        flag("--privileged", None);
    }
    for cap in strings(&host["CapAdd"]) {
        flag("--cap-add", Some(cap));
    }
    for cap in strings(&host["CapDrop"]) {
        flag("--cap-drop", Some(cap));
    }
    for device in host["Devices"].as_array().into_iter().flatten() {
        let on_host = device["PathOnHost"].as_str().unwrap_or_default();
        let inside = device["PathInContainer"].as_str().unwrap_or_default();
        flag("--device", Some(format!("{on_host}:{inside}")));
    }
    if host["DeviceRequests"]
        .as_array()
        .is_some_and(|r| !r.is_empty())
    {
        flag("--gpus", None);
    }
    for opt in strings(&host["SecurityOpt"]) {
        flag("--security-opt", Some(opt));
    }
    for group in strings(&host["GroupAdd"]) {
        flag("--group-add", Some(group));
    }
    for entry in strings(&host["ExtraHosts"]) {
        flag("--add-host", Some(entry));
    }
    for server in strings(&host["Dns"]) {
        flag("--dns", Some(server));
    }
    for domain in strings(&host["DnsSearch"]) {
        flag("--dns-search", Some(domain));
    }
    for opt in strings(&host["DnsOptions"]) {
        flag("--dns-option", Some(opt));
    }
    let mut sysctls: Vec<String> = host["Sysctls"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
        .collect();
    sysctls.sort();
    for sysctl in sysctls {
        flag("--sysctl", Some(sysctl));
    }
    for limit in host["Ulimits"].as_array().into_iter().flatten() {
        let name = limit["Name"].as_str().unwrap_or_default();
        let (soft, hard) = (
            limit["Soft"].as_i64().unwrap_or(0),
            limit["Hard"].as_i64().unwrap_or(0),
        );
        flag("--ulimit", Some(format!("{name}={soft}:{hard}")));
    }
    for (key, mode) in [
        ("PidMode", "--pid"),
        ("UTSMode", "--uts"),
        ("UsernsMode", "--userns"),
    ] {
        if let Some(value) = text(&host[key]) {
            flag(mode, Some(value));
        }
    }
    // `private` and `shareable` are the daemon's defaults.
    if let Some(ipc) = text(&host["IpcMode"]).filter(|m| m != "private" && m != "shareable") {
        flag("--ipc", Some(ipc));
    }
    if let Some(runtime) =
        text(&host["Runtime"]).filter(|r| r != "runc" && !r.starts_with("io.containerd.runc"))
    {
        flag("--runtime", Some(runtime));
    }
    if on(&host["Init"]) {
        flag("--init", None);
    }
    if on(&host["ReadonlyRootfs"]) {
        flag("--read-only", None);
    }
    if on(&host["AutoRemove"]) {
        flag("--rm", None);
    }
    if on(&host["OomKillDisable"]) {
        flag("--oom-kill-disable", None);
    }
    let mut log_opts: Vec<String> = host["LogConfig"]["Config"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
        .collect();
    log_opts.sort();
    for opt in log_opts {
        flag("--log-opt", Some(opt));
    }
    out
}

/// Bytes as `docker run` takes them: `512m`, `2g`, or a plain count.
fn size(bytes: i64) -> String {
    const K: i64 = 1024;
    match bytes {
        b if b % (K * K * K) == 0 => format!("{}g", b / (K * K * K)),
        b if b % (K * K) == 0 => format!("{}m", b / (K * K)),
        b if b % K == 0 => format!("{}k", b / K),
        b => b.to_string(),
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_owned))
        .collect()
}

/// `HostConfig.PortBindings`: `{"80/tcp": [{"HostIp": "", "HostPort": "8080"}]}`.
fn ports(bindings: &Value) -> Vec<PublishedPort> {
    let mut out = Vec::new();
    for (key, list) in bindings.as_object().into_iter().flatten() {
        let (port, protocol) = key.split_once('/').unwrap_or((key.as_str(), "tcp"));
        let Ok(container_port) = port.parse::<u16>() else {
            continue;
        };
        let list = list.as_array().cloned().unwrap_or_default();
        // An exposed port with no binding entry is published on a free host port.
        let list = if list.is_empty() {
            vec![Value::Null]
        } else {
            list
        };
        for b in list {
            let host_ip = b["HostIp"]
                .as_str()
                .filter(|ip| !ip.is_empty())
                .map(str::to_owned);
            let host_port = b["HostPort"].as_str().and_then(|p| p.parse::<u16>().ok());
            out.push(PublishedPort {
                host_ip,
                host_port: host_port.filter(|p| *p > 0),
                container_port,
                protocol: protocol.to_owned(),
            });
        }
    }
    out.sort_by(|a, b| {
        (a.container_port, &a.protocol, a.host_port, &a.host_ip).cmp(&(
            b.container_port,
            &b.protocol,
            b.host_port,
            &b.host_ip,
        ))
    });
    out
}

/// Docker names an anonymous volume with 64 hex digits; a copy gets a fresh one.
fn is_anonymous(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn image() -> Value {
        json!({
            "Config": {
                "Env": ["PATH=/usr/local/bin:/usr/bin", "NGINX_VERSION=1.27"],
                "Cmd": ["nginx", "-g", "daemon off;"],
                "Entrypoint": ["/docker-entrypoint.sh"]
            }
        })
    }

    fn container() -> Value {
        let anon = "a".repeat(64);
        json!({
            "Name": "/web",
            "Config": {
                "Image": "nginx:1.27",
                "Env": ["PATH=/usr/local/bin:/usr/bin", "NGINX_VERSION=1.27", "MODE=prod", "EMPTY="],
                "Cmd": ["nginx", "-g", "daemon off;"],
                "Entrypoint": ["/docker-entrypoint.sh"]
            },
            "HostConfig": {
                "PortBindings": {
                    "80/tcp": [{"HostIp": "", "HostPort": "8080"}, {"HostIp": "::1", "HostPort": "8081"}],
                    "53/udp": [{"HostIp": "127.0.0.1", "HostPort": ""}],
                    "9000/tcp": null
                },
                "NetworkMode": "shop_default",
                "RestartPolicy": {"Name": "unless-stopped", "MaximumRetryCount": 0}
            },
            "Mounts": [
                {"Type": "volume", "Name": "data", "Source": "/var/lib/docker/volumes/data/_data", "Destination": "/data", "RW": true},
                {"Type": "volume", "Name": "cache", "Destination": "/cache", "RW": false},
                {"Type": "volume", "Name": anon, "Destination": "/var/cache/nginx", "RW": true},
                {"Type": "bind", "Source": "/home/me/site", "Destination": "/usr/share/nginx/html", "RW": true},
                {"Type": "tmpfs", "Source": "", "Destination": "/run"}
            ]
        })
    }

    #[test]
    fn template_carries_what_the_run_form_can_express() {
        let t = template_from_inspect(&container(), Some(&image()));
        let spec = t.spec;
        assert_eq!(spec.image, "nginx:1.27");
        assert_eq!(spec.name.as_deref(), Some("web"));
        // The image's own CMD and environment are not copied.
        assert!(spec.cmd.is_empty());
        assert_eq!(spec.env, ["MODE=prod", "EMPTY="]);
        assert_eq!(spec.network.as_deref(), Some("shop_default"));
        assert_eq!(spec.restart, RestartPolicy::UnlessStopped);
        let port = |ip: Option<&str>, host: Option<u16>, c: u16, proto: &str| PublishedPort {
            host_ip: ip.map(str::to_owned),
            host_port: host,
            container_port: c,
            protocol: proto.into(),
        };
        assert_eq!(
            spec.ports,
            [
                port(Some("127.0.0.1"), None, 53, "udp"),
                port(None, Some(8080), 80, "tcp"),
                port(Some("::1"), Some(8081), 80, "tcp"),
                port(None, None, 9000, "tcp"),
            ]
        );
        assert_eq!(
            spec.volumes,
            [
                VolumeMount {
                    volume: "cache".into(),
                    target: "/cache".into(),
                    read_only: true
                },
                VolumeMount {
                    volume: "data".into(),
                    target: "/data".into(),
                    read_only: false
                },
            ]
        );
    }

    #[test]
    fn template_lists_what_it_leaves_out() {
        let t = template_from_inspect(&container(), Some(&image()));
        assert_eq!(
            t.skipped,
            [
                SkippedSetting::Mount {
                    kind: "volume".into(),
                    source: None,
                    target: "/var/cache/nginx".into()
                },
                SkippedSetting::Mount {
                    kind: "bind".into(),
                    source: Some("/home/me/site".into()),
                    target: "/usr/share/nginx/html".into()
                },
                SkippedSetting::Mount {
                    kind: "tmpfs".into(),
                    source: None,
                    target: "/run".into()
                },
            ]
        );
    }

    #[test]
    fn template_keeps_a_changed_command_and_reports_a_changed_entrypoint() {
        let mut c = container();
        c["Config"]["Cmd"] = json!(["nginx", "-T"]);
        c["Config"]["Entrypoint"] = json!(["/bin/sh", "-c"]);
        c["HostConfig"]["NetworkMode"] = json!("container:abc");
        c["HostConfig"]["RestartPolicy"]["Name"] = json!("on-failure");
        let t = template_from_inspect(&c, Some(&image()));
        assert_eq!(t.spec.cmd, ["nginx", "-T"]);
        assert_eq!(t.spec.network, None);
        assert_eq!(t.spec.restart, RestartPolicy::OnFailure);
        assert!(t.skipped.contains(&SkippedSetting::Entrypoint(vec![
            "/bin/sh".into(),
            "-c".into()
        ])));
        assert!(
            t.skipped
                .contains(&SkippedSetting::NetworkMode("container:abc".into()))
        );
    }

    #[test]
    fn template_without_the_image_keeps_the_whole_command_and_environment() {
        // The image may be gone (removed with `-f`): nothing is subtracted then.
        let t = template_from_inspect(&container(), None);
        assert_eq!(t.spec.cmd, ["nginx", "-g", "daemon off;"]);
        assert_eq!(t.spec.env.len(), 4);
        assert!(
            !t.skipped
                .iter()
                .any(|s| matches!(s, SkippedSetting::Entrypoint(_)))
        );
    }

    #[test]
    fn every_setting_the_form_cannot_carry_is_listed_as_its_flag() {
        let mut c = container();
        c["Id"] = json!("0123456789abcdef");
        let config = c["Config"].as_object_mut().unwrap();
        config.insert("Hostname".into(), json!("api.local"));
        config.insert("User".into(), json!("1000:1000"));
        config.insert("Tty".into(), json!(true));
        config.insert("OpenStdin".into(), json!(true));
        config.insert(
            "Labels".into(),
            json!({"team": "web", "maintainer": "nginx", "com.docker.compose.project": "shop"}),
        );
        config.insert("ExposedPorts".into(), json!({"80/tcp": {}, "7000/tcp": {}}));
        config.insert("StopTimeout".into(), json!(30));
        let host = c["HostConfig"].as_object_mut().unwrap();
        for (key, value) in [
            ("Memory", json!(536870912)),
            ("MemorySwap", json!(2147483648_i64)),
            ("NanoCpus", json!(1_500_000_000)),
            ("PidsLimit", json!(100)),
            ("ShmSize", json!(67108864)),
            ("Privileged", json!(true)),
            ("CapAdd", json!(["NET_ADMIN"])),
            ("CapDrop", json!(null)),
            (
                "Devices",
                json!([{"PathOnHost": "/dev/fuse", "PathInContainer": "/dev/fuse"}]),
            ),
            ("ExtraHosts", json!(["host.docker.internal:host-gateway"])),
            ("Sysctls", json!({"net.core.somaxconn": "1024"})),
            (
                "Ulimits",
                json!([{"Name": "nofile", "Soft": 1024, "Hard": 2048}]),
            ),
            ("IpcMode", json!("private")),
            ("PidMode", json!("")),
            ("Runtime", json!("runc")),
            ("Init", json!(true)),
            (
                "LogConfig",
                json!({"Type": "json-file", "Config": {"max-size": "10m"}}),
            ),
        ] {
            host.insert(key.into(), value);
        }
        let mut image = image();
        image["Config"]["Labels"] = json!({"maintainer": "nginx"});
        image["Config"]["ExposedPorts"] = json!({"80/tcp": {}});
        let options: Vec<String> = template_from_inspect(&c, Some(&image))
            .skipped
            .into_iter()
            .filter_map(|s| match s {
                SkippedSetting::Option(flag) => Some(flag),
                _ => None,
            })
            .collect();
        assert_eq!(
            options,
            [
                "-t",
                "-i",
                "--user 1000:1000",
                "--hostname api.local",
                "--label team=web",
                "--expose 7000/tcp",
                "--stop-timeout 30",
                "--memory 512m",
                "--memory-swap 2g",
                "--cpus 1.5",
                "--pids-limit 100",
                "--privileged",
                "--cap-add NET_ADMIN",
                "--device /dev/fuse:/dev/fuse",
                "--add-host host.docker.internal:host-gateway",
                "--sysctl net.core.somaxconn=1024",
                "--ulimit nofile=1024:2048",
                "--init",
                "--log-opt max-size=10m",
            ]
        );
    }

    #[test]
    fn the_daemons_own_defaults_are_not_listed() {
        let mut c = container();
        c["Id"] = json!("0123456789abcdef");
        c["Config"]["Hostname"] = json!("0123456789ab");
        c["Config"]["Tty"] = json!(false);
        let host = c["HostConfig"].as_object_mut().unwrap();
        for (key, value) in [
            ("Memory", json!(268435456)),
            // Twice the memory: what the daemon sets when no swap limit is given.
            ("MemorySwap", json!(536870912)),
            ("ShmSize", json!(67108864)),
            ("Privileged", json!(false)),
            ("IpcMode", json!("shareable")),
            ("Runtime", json!("io.containerd.runc.v2")),
            ("LogConfig", json!({"Type": "json-file", "Config": {}})),
            ("CpuShares", json!(0)),
            ("PidsLimit", json!(null)),
        ] {
            host.insert(key.into(), value);
        }
        let t = template_from_inspect(&c, Some(&image()));
        let options: Vec<_> = t
            .skipped
            .iter()
            .filter(|s| matches!(s, SkippedSetting::Option(_)))
            .collect();
        assert_eq!(options, [&SkippedSetting::Option("--memory 256m".into())]);
    }

    #[test]
    fn sizes_read_like_the_cli() {
        assert_eq!(size(512 << 20), "512m");
        assert_eq!(size(2 << 30), "2g");
        assert_eq!(size(1536), "1536");
        assert_eq!(size(4096), "4k");
        assert_eq!(size(1000), "1000");
    }

    #[test]
    fn a_copy_runs_the_same_image_even_when_the_tag_has_moved() {
        let (old, new) = ("sha256:old", "sha256:new");
        assert_eq!(
            image_to_run("nginx:1.27", Some(old), Some(old)),
            "nginx:1.27"
        );
        // Pulled again: the tag names another image now.
        assert_eq!(image_to_run("nginx:1.27", Some(new), Some(old)), old);
        // The tag is gone.
        assert_eq!(image_to_run("nginx:1.27", None, Some(old)), old);
        // Nothing to compare with: keep what the container was created with.
        assert_eq!(image_to_run("nginx:1.27", None, None), "nginx:1.27");
    }

    #[test]
    fn template_of_a_default_bridge_container_has_no_network_or_policy() {
        let c = json!({
            "Name": "/x",
            "Config": {"Image": "alpine"},
            "HostConfig": {"NetworkMode": "default", "RestartPolicy": {"Name": "no"}},
            "Mounts": []
        });
        let t = template_from_inspect(&c, None);
        assert_eq!(t.spec.network, None);
        assert_eq!(t.spec.restart, RestartPolicy::No);
        assert!(t.spec.ports.is_empty() && t.spec.volumes.is_empty() && t.skipped.is_empty());
    }
}
