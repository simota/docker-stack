//! Engine operations. The only place bollard models are converted.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::time::Duration;

use bollard::container::LogOutput;
use bollard::models::{
    ContainerCreateBody, ContainerSummaryStateEnum as S, EventMessageTypeEnum as T, HostConfig,
    Mount, MountType, NetworkCreateRequest, PortBinding, RestartPolicyNameEnum,
    VolumeCreateRequest,
};
use bollard::query_parameters as q;
use futures::StreamExt;

use crate::model::*;
use crate::runtime::{EngineStream, EngineTask, pump, spawn};
use crate::{Engine, EngineError};

type R<T> = Result<T, EngineError>;

const STREAM_CAPACITY: usize = 256;
const OP_TIMEOUT: Duration = Duration::from_secs(120);
const STATS_SWEEP_CONCURRENCY: usize = 8;

fn container_mem_usage(mem: &bollard::models::ContainerMemoryStats) -> u64 {
    // Match `docker stats`: subtract page cache (cgroup v2 `inactive_file`).
    let cache = mem
        .stats
        .as_ref()
        .and_then(|m| {
            m.get("inactive_file")
                .or_else(|| m.get("total_inactive_file"))
        })
        .copied()
        .unwrap_or(0);
    mem.usage.unwrap_or(0).saturating_sub(cache)
}

#[cfg(unix)]
pub(crate) fn local_fs_total_bytes(path: Option<&str>) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let raw = path?.trim();
    if raw.is_empty() {
        return None;
    }
    let c_path = CString::new(std::path::Path::new(raw).as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `c_path` is a valid NUL-terminated path and `stat` is valid writable storage.
    if unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: `statvfs` succeeded and initialized `stat`.
    let stat = unsafe { stat.assume_init() };
    // statvfs field widths differ by platform (macOS: `f_blocks` is u32).
    fn widen<T: Into<u64>>(v: T) -> u64 {
        v.into()
    }
    let block_size = widen(if stat.f_frsize > 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    });
    let total = widen(stat.f_blocks).checked_mul(block_size)?;
    (total > 0).then_some(total)
}

#[cfg(not(unix))]
pub(crate) fn local_fs_total_bytes(_path: Option<&str>) -> Option<u64> {
    None
}

fn spawn_op<T, F>(host: String, fut: F) -> EngineTask<R<T>>
where
    T: Send + 'static,
    F: Future<Output = R<T>> + Send + 'static,
{
    spawn_op_with_timeout(OP_TIMEOUT, host, fut)
}

pub(crate) fn spawn_op_with_timeout<T, F>(
    timeout: Duration,
    host: String,
    fut: F,
) -> EngineTask<R<T>>
where
    T: Send + 'static,
    F: Future<Output = R<T>> + Send + 'static,
{
    spawn(async move { with_timeout(timeout, &host, fut).await })
}

async fn with_timeout<T, F>(timeout: Duration, host: &str, fut: F) -> R<T>
where
    F: Future<Output = R<T>>,
{
    match tokio::time::timeout(timeout, fut).await {
        Ok(res) => res,
        Err(_) => Err(EngineError::Timeout(format!(
            "engine request to {host} timed out after {}s",
            timeout.as_secs().max(1)
        ))),
    }
}

fn image_prune_filter(mode: CleanupImages) -> HashMap<String, Vec<String>> {
    let dangling = match mode {
        CleanupImages::Dangling => "true",
        CleanupImages::AllUnused => "false",
    };
    HashMap::from([("dangling".to_string(), vec![dangling.to_string()])])
}

fn volume_prune_options(
    mode: CleanupVolumes,
    api_version: &str,
) -> R<Option<q::PruneVolumesOptions>> {
    let legacy = prune_removes_named_volumes(Some(api_version));
    match (mode, legacy) {
        (CleanupVolumes::Anonymous, true) => Err(EngineError::Local(
            "Docker Engine API < 1.42 cannot prune anonymous volumes without removing named volumes."
                .into(),
        )),
        (CleanupVolumes::Anonymous, false) | (CleanupVolumes::AllUnused, true) => Ok(None),
        (CleanupVolumes::AllUnused, false) => Ok(Some(q::PruneVolumesOptions {
            filters: Some(HashMap::from([(
                "all".to_string(),
                vec!["true".to_string()],
            )])),
        })),
    }
}

pub(crate) fn event_filters() -> HashMap<String, Vec<String>> {
    HashMap::from([(
        "type".to_string(),
        vec![
            "container".to_string(),
            "image".to_string(),
            "volume".to_string(),
            "network".to_string(),
        ],
    )])
}

fn convert_event(m: bollard::models::EventMessage) -> EngineEvent {
    let kind = match m.typ {
        Some(T::CONTAINER) => ResourceKind::Container,
        Some(T::IMAGE) => ResourceKind::Image,
        Some(T::VOLUME) => ResourceKind::Volume,
        Some(T::NETWORK) => ResourceKind::Network,
        _ => ResourceKind::Other,
    };
    let (id, mut attributes) = m
        .actor
        .map(|a| (a.id.unwrap_or_default(), a.attributes.unwrap_or_default()))
        .unwrap_or_default();
    EngineEvent {
        kind,
        action: m.action.unwrap_or_default(),
        id,
        name: attributes
            .remove("name")
            .map(|n| n.trim_start_matches('/').to_owned())
            .filter(|n| !n.is_empty()),
        exit_code: attributes
            .get("exitCode")
            .and_then(|c| c.trim().parse().ok()),
    }
}

fn convert_container(c: bollard::models::ContainerSummary) -> ContainerSummary {
    let state = match c.state {
        Some(S::CREATED) => ContainerState::Created,
        Some(S::RUNNING) => ContainerState::Running,
        Some(S::PAUSED) => ContainerState::Paused,
        Some(S::RESTARTING) => ContainerState::Restarting,
        Some(S::EXITED) => ContainerState::Exited,
        Some(S::REMOVING) => ContainerState::Removing,
        Some(S::DEAD) => ContainerState::Dead,
        _ => ContainerState::Unknown,
    };
    let labels = c.labels.unwrap_or_default();
    let compose = labels
        .get("com.docker.compose.project")
        .map(|project| ComposeLabels {
            project: project.clone(),
            service: labels
                .get("com.docker.compose.service")
                .cloned()
                .unwrap_or_default(),
            working_dir: labels
                .get("com.docker.compose.project.working_dir")
                .cloned(),
            config_files: labels
                .get("com.docker.compose.project.config_files")
                .map(|s| {
                    s.split(',')
                        .filter(|f| !f.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        });
    let mut ports: Vec<PortMapping> = c
        .ports
        .unwrap_or_default()
        .into_iter()
        .map(|p| PortMapping {
            ip: p.ip.filter(|ip| !ip.is_empty()),
            private_port: p.private_port,
            public_port: p.public_port,
            protocol: p.typ.map(|t| t.to_string()).unwrap_or_else(|| "tcp".into()),
        })
        .collect();
    dedup_dual_stack(&mut ports);
    let mut networks: Vec<String> = c
        .network_settings
        .and_then(|n| n.networks)
        .map(|n| n.into_keys().collect())
        .unwrap_or_default();
    networks.sort();
    let volumes = c
        .mounts
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.typ.as_deref() == Some("volume"))
        .filter_map(|m| m.name)
        .collect();
    ContainerSummary {
        id: c.id.unwrap_or_default(),
        name: c
            .names
            .and_then(|n| n.into_iter().next())
            .map(|n| n.trim_start_matches('/').to_owned())
            .unwrap_or_default(),
        image: c.image.unwrap_or_default(),
        state,
        status: c.status.unwrap_or_default(),
        created: c.created.unwrap_or_default(),
        ports,
        compose,
        networks,
        volumes,
    }
}

/// dockerd lists a port published on every address twice, as `0.0.0.0` and `::`. Drops
/// the `::` entry when the `0.0.0.0` one exists for the same port and protocol. Every
/// other address stays, so the list never shows a port reachable on fewer addresses
/// than it is. Sorts by container port, host port and protocol.
fn dedup_dual_stack(ports: &mut Vec<PortMapping>) {
    let ip_is = |p: &PortMapping, ip: &str| p.ip.as_deref() == Some(ip);
    let key = |p: &PortMapping| (p.private_port, p.public_port, p.protocol.clone());
    let on_all_ipv4: HashSet<_> = ports
        .iter()
        .filter(|p| ip_is(p, "0.0.0.0"))
        .map(key)
        .collect();
    ports.retain(|p| !(ip_is(p, "::") && on_all_ipv4.contains(&key(p))));
    ports.sort_by(|a, b| {
        (a.private_port, a.public_port, &a.protocol).cmp(&(
            b.private_port,
            b.public_port,
            &b.protocol,
        ))
    });
}

/// Sorted by name: dockerd lists volumes in no fixed order, and a stable order lets an
/// unchanged list compare equal to the last one.
fn convert_volumes(volumes: Vec<bollard::models::Volume>) -> Vec<VolumeSummary> {
    let mut out: Vec<VolumeSummary> = volumes
        .into_iter()
        .map(|v| VolumeSummary {
            name: v.name,
            driver: v.driver,
            mountpoint: v.mountpoint,
            created_at: v.created_at,
            labels: v.labels.into_iter().collect::<BTreeMap<_, _>>(),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// `{Name, UsageData: {Size}}` items from `/system/df?verbose=1`; size -1 means not computed.
fn volume_sizes_from_items(items: &[serde_json::Value]) -> BTreeMap<String, u64> {
    items
        .iter()
        .filter_map(|v| {
            let name = v.get("Name")?.as_str()?.to_owned();
            let size = v.pointer("/UsageData/Size")?.as_i64().filter(|s| *s >= 0)?;
            Some((name, size as u64))
        })
        .collect()
}

/// The create request for `spec`: published ports, named-volume mounts, the network
/// and the restart policy go in the host config, as `docker run` puts them.
fn create_body(spec: &RunSpec) -> ContainerCreateBody {
    let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
    for p in &spec.ports {
        let key = format!("{}/{}", p.container_port, p.protocol);
        port_bindings
            .entry(key)
            .or_insert_with(|| Some(Vec::new()))
            .get_or_insert_with(Vec::new)
            .push(PortBinding {
                host_ip: p.host_ip.clone(),
                host_port: Some(p.host_port.map(|n| n.to_string()).unwrap_or_default()),
            });
    }
    let mut exposed: Vec<String> = port_bindings.keys().cloned().collect();
    exposed.sort();
    let mounts: Vec<Mount> = spec
        .volumes
        .iter()
        .map(|v| Mount {
            typ: Some(MountType::VOLUME),
            source: Some(v.volume.clone()),
            target: Some(v.target.clone()),
            read_only: Some(v.read_only),
            ..Default::default()
        })
        .collect();
    let restart = match spec.restart {
        RestartPolicy::No => RestartPolicyNameEnum::NO,
        RestartPolicy::Always => RestartPolicyNameEnum::ALWAYS,
        RestartPolicy::UnlessStopped => RestartPolicyNameEnum::UNLESS_STOPPED,
        RestartPolicy::OnFailure => RestartPolicyNameEnum::ON_FAILURE,
    };
    let host_config = HostConfig {
        port_bindings: (!port_bindings.is_empty()).then_some(port_bindings),
        mounts: (!mounts.is_empty()).then_some(mounts),
        network_mode: spec.network.clone(),
        restart_policy: Some(bollard::models::RestartPolicy {
            name: Some(restart),
            maximum_retry_count: None,
        }),
        ..Default::default()
    };
    ContainerCreateBody {
        image: Some(spec.image.clone()),
        cmd: (!spec.cmd.is_empty()).then(|| spec.cmd.clone()),
        env: (!spec.env.is_empty()).then(|| spec.env.clone()),
        exposed_ports: (!exposed.is_empty()).then_some(exposed),
        host_config: Some(host_config),
        ..Default::default()
    }
}

/// How long checking every container for a newer image may take.
const CHECK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How long an update may spend pulling its image.
const UPDATE_PULL_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// [`crate::recreate::Steps`] against the daemon.
struct DaemonSteps {
    docker: bollard::Docker,
    engine: Engine,
}

impl crate::recreate::Steps for DaemonSteps {
    async fn stop(&self, id: &str) -> R<()> {
        self.docker
            .stop_container(id, None::<q::StopContainerOptions>)
            .await
            .map_err(|e| self.engine.err(e))
    }

    async fn start(&self, id: &str) -> R<()> {
        self.docker
            .start_container(id, None::<q::StartContainerOptions>)
            .await
            .map_err(|e| self.engine.err(e))
    }

    async fn rename(&self, id: &str, name: &str) -> R<()> {
        let opts = q::RenameContainerOptions {
            name: name.to_owned(),
        };
        self.docker
            .rename_container(id, opts)
            .await
            .map_err(|e| self.engine.err(e))
    }

    async fn create(&self, name: &str, body: &serde_json::Value) -> R<String> {
        let body: ContainerCreateBody = serde_json::from_value(body.clone())
            .map_err(|e| EngineError::Local(format!("Could not build the new container: {e}")))?;
        let opts = q::CreateContainerOptions {
            name: Some(name.to_owned()),
            ..Default::default()
        };
        self.docker
            .create_container(Some(opts), body)
            .await
            .map(|created| created.id)
            .map_err(|e| self.engine.err(e))
    }

    async fn connect(&self, network: &str, id: &str, endpoint: &serde_json::Value) -> R<()> {
        let endpoint_config = serde_json::from_value(endpoint.clone()).map_err(|e| {
            EngineError::Local(format!("Could not keep the network {network}: {e}"))
        })?;
        let request = bollard::models::NetworkConnectRequest {
            container: id.to_owned(),
            endpoint_config: Some(endpoint_config),
        };
        self.docker
            .connect_network(network, request)
            .await
            .map_err(|e| self.engine.err(e))
    }

    async fn remove(&self, id: &str, force: bool) -> R<()> {
        let opts = q::RemoveContainerOptions {
            force,
            v: false,
            ..Default::default()
        };
        self.docker
            .remove_container(id, Some(opts))
            .await
            .map_err(|e| self.engine.err(e))
    }
}

impl Engine {
    pub fn ping(&self) -> EngineTask<R<()>> {
        let (d, this) = (self.docker.clone(), self.clone());
        spawn_op(self.target.host.clone(), async move {
            d.ping().await.map(|_| ()).map_err(|e| this.err(e))
        })
    }

    pub fn system_capacity(&self) -> EngineTask<R<SystemCapacity>> {
        let (d, this) = (self.docker.clone(), self.clone());
        spawn_op(self.target.host.clone(), async move {
            let info = d.info().await.map_err(|e| this.err(e))?;
            let is_local = this.target.host.starts_with("unix://");
            let root_dir = info.docker_root_dir;
            let disk_total = if is_local {
                tokio::task::spawn_blocking(move || local_fs_total_bytes(root_dir.as_deref()))
                    .await
                    .ok()
                    .flatten()
            } else {
                None
            };
            Ok(SystemCapacity {
                cpu_cores: info.ncpu.unwrap_or(0).max(0) as u32,
                mem_total: info.mem_total.unwrap_or(0).max(0) as u64,
                disk_total,
            })
        })
    }

    pub fn container_usage(&self, ids: Vec<String>) -> EngineTask<R<ContainerUsageSample>> {
        let (d, this) = (self.docker.clone(), self.clone());
        spawn_op(self.target.host.clone(), async move {
            if ids.is_empty() {
                return Ok(ContainerUsageSample::default());
            }
            let results: Vec<(String, R<Option<bollard::models::ContainerStatsResponse>>)> =
                futures::stream::iter(ids)
                    .map(|id| {
                        let (d, this) = (d.clone(), this.clone());
                        async move {
                            let opts = q::StatsOptions {
                                stream: false,
                                one_shot: true,
                            };
                            let mut stream = d.stats(&id, Some(opts));
                            let item = match stream.next().await {
                                Some(Ok(s)) => Ok(Some(s)),
                                Some(Err(e)) => Err(this.err(e)),
                                None => Ok(None),
                            };
                            (id, item)
                        }
                    })
                    .buffer_unordered(STATS_SWEEP_CONCURRENCY)
                    .collect()
                    .await;

            let mut out = ContainerUsageSample::default();
            for (id, item) in results {
                match item {
                    Ok(Some(s)) => {
                        if let Some(mem) = s.memory_stats.as_ref() {
                            out.mem_usage = out.mem_usage.saturating_add(container_mem_usage(mem));
                        }
                        let cpu = s.cpu_stats.unwrap_or_default();
                        if let (Some(total), Some(sys)) = (
                            cpu.cpu_usage.as_ref().and_then(|u| u.total_usage),
                            cpu.system_cpu_usage.filter(|&v| v > 0),
                        ) {
                            out.cpu_readings.insert(
                                id,
                                ContainerCpuReading {
                                    total_usage: total,
                                    system_usage: sys,
                                    online_cpus: cpu.online_cpus,
                                },
                            );
                        }
                    }
                    Ok(None) | Err(EngineError::NotFound(_) | EngineError::Conflict(_)) => {}
                    Err(e) if e.is_connection_lost() => return Err(e),
                    Err(e) => {
                        tracing::debug!(container = %id, error = %e, "skipping failed container stats sample");
                    }
                }
            }
            Ok(out)
        })
    }

    pub fn disk_usage(&self) -> EngineTask<R<Option<DiskUsage>>> {
        let (d, this) = (self.docker.clone(), self.clone());
        spawn_op(self.target.host.clone(), async move {
            // verbose lists volumes with their sizes (API 1.52+); older daemons ignore it.
            let opts = q::DataUsageOptions {
                verbose: true,
                ..Default::default()
            };
            let df = d.df(Some(opts)).await.map_err(|e| this.err(e))?;
            let has_usage = df.image_usage.is_some()
                || df.container_usage.is_some()
                || df.volume_usage.is_some()
                || df.build_cache_usage.is_some();
            if !has_usage {
                return Ok(None);
            }
            let non_neg = |v: Option<i64>| v.unwrap_or(0).max(0) as u64;
            let volume_sizes = df
                .volume_usage
                .as_ref()
                .and_then(|u| u.items.as_ref())
                .map(|items| volume_sizes_from_items(items))
                .unwrap_or_default();
            Ok(Some(DiskUsage {
                images: non_neg(df.image_usage.and_then(|u| u.total_size)),
                containers: non_neg(df.container_usage.and_then(|u| u.total_size)),
                volumes: non_neg(df.volume_usage.and_then(|u| u.total_size)),
                build_cache: non_neg(df.build_cache_usage.and_then(|u| u.total_size)),
                volume_sizes,
            }))
        })
    }

    // ---- containers ----

    pub fn containers(&self, all: bool) -> EngineTask<R<Vec<ContainerSummary>>> {
        let (d, this) = (self.docker.clone(), self.clone());
        spawn_op(self.target.host.clone(), async move {
            let opts = q::ListContainersOptions {
                all,
                ..Default::default()
            };
            let list = d
                .list_containers(Some(opts))
                .await
                .map_err(|e| this.err(e))?;
            Ok(list.into_iter().map(convert_container).collect())
        })
    }

    pub fn container_action(&self, id: &str, action: ContainerAction) -> EngineTask<R<()>> {
        let (d, this, id) = (self.docker.clone(), self.clone(), id.to_owned());
        spawn_op(self.target.host.clone(), async move {
            let r = match action {
                ContainerAction::Start => {
                    d.start_container(&id, None::<q::StartContainerOptions>)
                        .await
                }
                ContainerAction::Stop => {
                    d.stop_container(&id, None::<q::StopContainerOptions>).await
                }
                ContainerAction::Restart => {
                    d.restart_container(&id, None::<q::RestartContainerOptions>)
                        .await
                }
                ContainerAction::Kill => {
                    d.kill_container(&id, None::<q::KillContainerOptions>).await
                }
                ContainerAction::Pause => d.pause_container(&id).await,
                ContainerAction::Unpause => d.unpause_container(&id).await,
                ContainerAction::Remove { force, volumes } => {
                    let opts = q::RemoveContainerOptions {
                        force,
                        v: volumes,
                        ..Default::default()
                    };
                    d.remove_container(&id, Some(opts)).await
                }
            };
            r.map_err(|e| this.err(e))
        })
    }

    pub fn inspect_container(&self, id: &str) -> EngineTask<R<String>> {
        let (d, this, id) = (self.docker.clone(), self.clone(), id.to_owned());
        spawn_op(self.target.host.clone(), async move {
            let v = d
                .inspect_container(&id, None::<q::InspectContainerOptions>)
                .await
                .map_err(|e| this.err(e))?;
            serde_json::to_string_pretty(&v).map_err(|e| EngineError::Local(e.to_string()))
        })
    }

    /// The settings to run another container like `id` with (see [`ContainerTemplate`]).
    pub fn container_template(&self, id: &str) -> EngineTask<R<ContainerTemplate>> {
        let (d, this, id) = (self.docker.clone(), self.clone(), id.to_owned());
        spawn_op(self.target.host.clone(), async move {
            let c = d
                .inspect_container(&id, None::<q::InspectContainerOptions>)
                .await
                .map_err(|e| this.err(e))?;
            // Without its image (removed by force) the whole command and environment
            // are kept.
            let image = match c.image.as_deref() {
                Some(image) => d.inspect_image(image).await.ok(),
                None => None,
            };
            let json_err = |e: serde_json::Error| EngineError::Local(e.to_string());
            let container = serde_json::to_value(&c).map_err(json_err)?;
            let image = image
                .map(|i| serde_json::to_value(&i))
                .transpose()
                .map_err(json_err)?;
            let mut template = crate::template::template_from_inspect(&container, image.as_ref());
            // The tag may point at another image by now (pulled again, or retagged):
            // run the image the container was made from.
            let tag_id = match template.spec.image.as_str() {
                "" => None,
                reference => d.inspect_image(reference).await.ok().and_then(|i| i.id),
            };
            template.spec.image = crate::template::image_to_run(
                &template.spec.image,
                tag_id.as_deref(),
                c.image.as_deref(),
            );
            Ok(template)
        })
    }

    /// Pulls the image the container `id` was created from (its tag) and, when the tag
    /// now names another image, replaces the container with one made from it, keeping
    /// its settings, volumes and networks (see [`crate::recreate`]). A failed pull of
    /// an image that is newer locally (built or pulled since) still updates.
    ///
    /// The replacement runs without an overall timeout (each request has the client's
    /// own), so it is never dropped between stopping the old container and starting
    /// the new one; only the pull is bounded, by [`UPDATE_PULL_TIMEOUT`].
    pub fn update_container(&self, id: &str) -> EngineTask<R<UpdateOutcome>> {
        let (d, this, id) = (self.docker.clone(), self.clone(), id.to_owned());
        spawn(async move {
            let c = d
                .inspect_container(&id, None::<q::InspectContainerOptions>)
                .await
                .map_err(|e| this.err(e))?;
            let json_err = |e: serde_json::Error| EngineError::Local(e.to_string());
            let container = serde_json::to_value(&c).map_err(json_err)?;
            let reference = crate::recreate::reference(&container)?;
            let running_image = c.image.clone().unwrap_or_default();
            let pulled = if crate::recreate::is_pinned(&reference) {
                Ok(())
            } else {
                let pull = this.pull_to_end(&reference);
                match tokio::time::timeout(UPDATE_PULL_TIMEOUT, pull).await {
                    Ok(r) => r,
                    Err(_) => Err(EngineError::Timeout(format!(
                        "pulling {reference} took longer than {}s",
                        UPDATE_PULL_TIMEOUT.as_secs()
                    ))),
                }
            };
            let latest = match d.inspect_image(&reference).await {
                Ok(image) => image.id.unwrap_or_default(),
                // Neither pulled nor here: the pull's error says why.
                Err(e) => return Err(pulled.err().unwrap_or_else(|| this.err(e))),
            };
            if latest == running_image {
                pulled?;
                return Ok(UpdateOutcome::UpToDate);
            }
            if let Err(e) = &pulled {
                tracing::info!(image = %reference, error = %e, "pull failed; updating to the newer local image");
            }
            let old_image = match d.inspect_image(&running_image).await {
                Ok(image) => Some(serde_json::to_value(&image).map_err(json_err)?),
                Err(_) => None,
            };
            let plan = crate::recreate::plan(&container, old_image.as_ref(), &reference)?;
            let old = c.id.clone().unwrap_or(id);
            let steps = DaemonSteps {
                docker: d,
                engine: this,
            };
            let replaced = crate::recreate::replace(&steps, &old, &plan).await?;
            tracing::info!(container = %plan.name, image = %reference, "updated container");
            Ok(UpdateOutcome::Updated {
                id: replaced.id,
                leftover: replaced.leftover,
            })
        })
    }

    /// Compares each container's image with the newest one for the tag it was created
    /// from, asking the registry (through the daemon) for the tag's digest once per
    /// tag. Nothing is pulled. A container that cannot be checked is
    /// [`ImageFreshness::Unknown`] rather than failing the rest.
    pub fn check_updates(&self, ids: Vec<String>) -> EngineTask<R<Vec<UpdateCheck>>> {
        let (d, this) = (self.docker.clone(), self.clone());
        spawn_op_with_timeout(CHECK_TIMEOUT, self.target.host.clone(), async move {
            // Container → (reference, image it runs).
            let inspected: Vec<(String, R<(String, String)>)> =
                futures::stream::iter(ids.into_iter().map(|id| {
                    let (d, this) = (d.clone(), this.clone());
                    async move {
                        let r = async {
                            let c = d
                                .inspect_container(&id, None::<q::InspectContainerOptions>)
                                .await
                                .map_err(|e| this.err(e))?;
                            let json = serde_json::to_value(&c)
                                .map_err(|e| EngineError::Local(e.to_string()))?;
                            let reference = crate::recreate::reference(&json)?;
                            Ok((reference, c.image.unwrap_or_default()))
                        }
                        .await;
                        (id, r)
                    }
                }))
                .buffer_unordered(STATS_SWEEP_CONCURRENCY)
                .collect()
                .await;
            let mut references: Vec<String> = inspected
                .iter()
                .filter_map(|(_, r)| r.as_ref().ok().map(|(reference, _)| reference.clone()))
                .collect();
            references.sort();
            references.dedup();
            // Tag → (what it names here, what the registry has).
            type Tag = (Option<(String, Vec<String>)>, Result<String, String>);
            let tags: HashMap<String, Tag> =
                futures::stream::iter(references.into_iter().map(|reference| {
                    let (d, this) = (d.clone(), this.clone());
                    async move {
                        let local = d.inspect_image(&reference).await.ok().map(|i| {
                            (i.id.unwrap_or_default(), i.repo_digests.unwrap_or_default())
                        });
                        let remote = if crate::recreate::is_pinned(&reference) {
                            // A digest names one image for good: compare it as is.
                            Ok(reference
                                .rsplit_once('@')
                                .map(|(_, d)| d.to_owned())
                                .unwrap_or_default())
                        } else {
                            d.inspect_registry_image(&reference, None)
                                .await
                                .map_err(|e| this.err(e).to_string())
                                .and_then(|r| {
                                    r.descriptor
                                        .digest
                                        .ok_or_else(|| "the registry sent no digest".to_owned())
                                })
                        };
                        (reference, (local, remote))
                    }
                }))
                .buffer_unordered(STATS_SWEEP_CONCURRENCY)
                .collect()
                .await;
            let mut checks: Vec<UpdateCheck> = inspected
                .into_iter()
                .map(|(container, r)| {
                    let freshness = match r {
                        Err(e) => ImageFreshness::Unknown(e.to_string()),
                        Ok((reference, running)) => match tags.get(&reference) {
                            Some((local, remote)) => crate::recreate::freshness(
                                &running,
                                local
                                    .as_ref()
                                    .map(|(id, digests)| (id.as_str(), digests.as_slice())),
                                remote.as_deref().map_err(Clone::clone),
                            ),
                            None => ImageFreshness::Unknown(String::new()),
                        },
                    };
                    UpdateCheck {
                        container,
                        freshness,
                    }
                })
                .collect();
            checks.sort_by(|a, b| a.container.cmp(&b.container));
            Ok(checks)
        })
    }

    /// Pulls `reference` until the daemon is done; the first error ends it.
    async fn pull_to_end(&self, reference: &str) -> R<()> {
        let mut stream = self.pull(reference);
        while let Some(item) = stream.next().await {
            item?;
        }
        Ok(())
    }

    /// Creates a container from `spec` and starts it; returns its id. When it does not
    /// start (a port already taken, say) the new container is removed again, so the
    /// same name can be used on the next try.
    pub fn run_container(&self, spec: RunSpec) -> EngineTask<R<String>> {
        let (d, this) = (self.docker.clone(), self.clone());
        spawn_op(self.target.host.clone(), async move {
            let opts = q::CreateContainerOptions {
                name: spec.name.clone(),
                ..Default::default()
            };
            let created = d
                .create_container(Some(opts), create_body(&spec))
                .await
                .map_err(|e| this.err(e))?;
            let id = created.id;
            if let Err(e) = d
                .start_container(&id, None::<q::StartContainerOptions>)
                .await
            {
                let opts = q::RemoveContainerOptions {
                    force: true,
                    v: true,
                    ..Default::default()
                };
                if let Err(rm) = d.remove_container(&id, Some(opts)).await {
                    tracing::debug!(container = %id, error = %rm, "could not remove a container that failed to start");
                }
                return Err(this.err(e));
            }
            Ok(id)
        })
    }

    /// Streams logs. `tail` is the number of past lines to include first.
    pub fn logs(&self, id: &str, tail: usize, follow: bool) -> EngineStream<R<LogChunk>> {
        let this = self.clone();
        let opts = q::LogsOptions {
            follow,
            stdout: true,
            stderr: true,
            tail: tail.to_string(),
            ..Default::default()
        };
        let stream = self.docker.logs(id, Some(opts));
        pump(STREAM_CAPACITY, stream, move |item| {
            Some(item.map_err(|e| this.err(e)).map(|out| {
                let (stream, bytes) = match out {
                    LogOutput::StdOut { message } => (LogStream::Stdout, message),
                    LogOutput::StdErr { message } => (LogStream::Stderr, message),
                    LogOutput::StdIn { message } | LogOutput::Console { message } => {
                        (LogStream::Console, message)
                    }
                };
                LogChunk {
                    stream,
                    text: String::from_utf8_lossy(&bytes).into_owned(),
                }
            }))
        })
    }

    pub fn stats(&self, id: &str) -> EngineStream<R<StatsSample>> {
        let this = self.clone();
        let opts = q::StatsOptions {
            stream: true,
            one_shot: false,
        };
        let stream = self.docker.stats(id, Some(opts));
        pump(8, stream, move |item| {
            Some(item.map_err(|e| this.err(e)).map(|s| {
                let cpu = s.cpu_stats.unwrap_or_default();
                let pre = s.precpu_stats.unwrap_or_default();
                let mem = s.memory_stats.unwrap_or_default();
                let (rx, tx) = s
                    .networks
                    .unwrap_or_default()
                    .values()
                    .fold((0, 0), |(r, t), n| {
                        (r + n.rx_bytes.unwrap_or(0), t + n.tx_bytes.unwrap_or(0))
                    });
                StatsSample {
                    cpu_percent: cpu_percent(
                        cpu.cpu_usage
                            .as_ref()
                            .and_then(|u| u.total_usage)
                            .unwrap_or(0),
                        pre.cpu_usage
                            .as_ref()
                            .and_then(|u| u.total_usage)
                            .unwrap_or(0),
                        cpu.system_cpu_usage.unwrap_or(0),
                        pre.system_cpu_usage.unwrap_or(0),
                        cpu.online_cpus.unwrap_or(1),
                    ),
                    mem_usage: container_mem_usage(&mem),
                    mem_limit: mem.limit.unwrap_or(0),
                    net_rx: rx,
                    net_tx: tx,
                    pids: s.pids_stats.and_then(|p| p.current).unwrap_or(0),
                }
            }))
        })
    }

    // ---- images ----

    pub fn images(&self) -> EngineTask<R<Vec<ImageSummary>>> {
        let (d, this) = (self.docker.clone(), self.clone());
        spawn_op(self.target.host.clone(), async move {
            let list = d
                .list_images(None::<q::ListImagesOptions>)
                .await
                .map_err(|e| this.err(e))?;
            Ok(list
                .into_iter()
                .map(|i| ImageSummary {
                    id: i.id,
                    tags: i
                        .repo_tags
                        .into_iter()
                        .filter(|t| t != "<none>:<none>")
                        .collect(),
                    size: i.size,
                    created: i.created,
                    containers: i.containers,
                })
                .collect())
        })
    }

    pub fn pull(&self, reference: &str) -> EngineStream<R<PullProgress>> {
        let this = self.clone();
        let (image, tag) = match split_reference(reference) {
            Ok(parts) => parts,
            Err(e) => return pump(1, futures::stream::iter([Err(e)]), Some),
        };
        let opts = q::CreateImageOptions {
            from_image: Some(image),
            tag: Some(tag),
            ..Default::default()
        };
        let stream = self.docker.create_image(Some(opts), None, None);
        pump(STREAM_CAPACITY, stream, move |item| {
            Some(match item {
                Err(e) => Err(this.err(e)),
                Ok(info) => match info.error_detail.and_then(|d| d.message) {
                    Some(msg) => Err(EngineError::Daemon {
                        status: 500,
                        message: msg,
                    }),
                    None => Ok(PullProgress {
                        layer: info.id,
                        status: info.status.unwrap_or_default(),
                        current: info.progress_detail.as_ref().and_then(|p| p.current),
                        total: info.progress_detail.as_ref().and_then(|p| p.total),
                    }),
                },
            })
        })
    }

    pub fn remove_image(&self, id: &str, force: bool) -> EngineTask<R<()>> {
        let (d, this, id) = (self.docker.clone(), self.clone(), id.to_owned());
        spawn_op(self.target.host.clone(), async move {
            let opts = q::RemoveImageOptions {
                force,
                ..Default::default()
            };
            d.remove_image(&id, Some(opts), None)
                .await
                .map(|_| ())
                .map_err(|e| this.err(e))
        })
    }

    pub fn inspect_image(&self, id: &str) -> EngineTask<R<String>> {
        let (d, this, id) = (self.docker.clone(), self.clone(), id.to_owned());
        spawn_op(self.target.host.clone(), async move {
            let v = d.inspect_image(&id).await.map_err(|e| this.err(e))?;
            serde_json::to_string_pretty(&v).map_err(|e| EngineError::Local(e.to_string()))
        })
    }

    // ---- volumes ----

    pub fn volumes(&self) -> EngineTask<R<Vec<VolumeSummary>>> {
        let (d, this) = (self.docker.clone(), self.clone());
        spawn_op(self.target.host.clone(), async move {
            let resp = d
                .list_volumes(None::<q::ListVolumesOptions>)
                .await
                .map_err(|e| this.err(e))?;
            Ok(convert_volumes(resp.volumes.unwrap_or_default()))
        })
    }

    pub fn create_volume(&self, name: &str) -> EngineTask<R<()>> {
        let (d, this, name) = (self.docker.clone(), self.clone(), name.to_owned());
        spawn_op(self.target.host.clone(), async move {
            let req = VolumeCreateRequest {
                name: Some(name),
                ..Default::default()
            };
            d.create_volume(req)
                .await
                .map(|_| ())
                .map_err(|e| this.err(e))
        })
    }

    pub fn remove_volume(&self, name: &str) -> EngineTask<R<()>> {
        let (d, this, name) = (self.docker.clone(), self.clone(), name.to_owned());
        spawn_op(self.target.host.clone(), async move {
            d.remove_volume(&name, None::<q::RemoveVolumeOptions>)
                .await
                .map_err(|e| this.err(e))
        })
    }

    pub fn inspect_volume(&self, name: &str) -> EngineTask<R<String>> {
        let (d, this, name) = (self.docker.clone(), self.clone(), name.to_owned());
        spawn_op(self.target.host.clone(), async move {
            let v = d.inspect_volume(&name).await.map_err(|e| this.err(e))?;
            serde_json::to_string_pretty(&v).map_err(|e| EngineError::Local(e.to_string()))
        })
    }

    // ---- networks ----

    pub fn networks(&self) -> EngineTask<R<Vec<NetworkSummary>>> {
        let (d, this) = (self.docker.clone(), self.clone());
        spawn_op(self.target.host.clone(), async move {
            let list = d
                .list_networks(None::<q::ListNetworksOptions>)
                .await
                .map_err(|e| this.err(e))?;
            let mut out: Vec<NetworkSummary> = list
                .into_iter()
                .map(|n| NetworkSummary {
                    id: n.id.unwrap_or_default(),
                    name: n.name.unwrap_or_default(),
                    driver: n.driver.unwrap_or_default(),
                    scope: n.scope.unwrap_or_default(),
                    internal: n.internal.unwrap_or(false),
                })
                .collect();
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        })
    }

    pub fn create_network(&self, name: &str) -> EngineTask<R<()>> {
        let (d, this, name) = (self.docker.clone(), self.clone(), name.to_owned());
        spawn_op(self.target.host.clone(), async move {
            let req = NetworkCreateRequest {
                name,
                driver: Some("bridge".into()),
                ..Default::default()
            };
            d.create_network(req)
                .await
                .map(|_| ())
                .map_err(|e| this.err(e))
        })
    }

    pub fn remove_network(&self, id: &str) -> EngineTask<R<()>> {
        let (d, this, id) = (self.docker.clone(), self.clone(), id.to_owned());
        spawn_op(self.target.host.clone(), async move {
            d.remove_network(&id).await.map_err(|e| this.err(e))
        })
    }

    /// The network with the containers attached to it.
    /// Attaches `container` to `network`, as `docker network connect` does.
    pub fn connect_network(&self, network: &str, container: &str) -> EngineTask<R<()>> {
        let (d, this) = (self.docker.clone(), self.clone());
        let (network, container) = (network.to_owned(), container.to_owned());
        spawn_op(self.target.host.clone(), async move {
            let request = bollard::models::NetworkConnectRequest {
                container,
                endpoint_config: Some(Default::default()),
            };
            d.connect_network(&network, request)
                .await
                .map_err(|e| this.err(e))
        })
    }

    /// Detaches `container` from `network`, as `docker network disconnect` does.
    pub fn disconnect_network(&self, network: &str, container: &str) -> EngineTask<R<()>> {
        let (d, this) = (self.docker.clone(), self.clone());
        let (network, container) = (network.to_owned(), container.to_owned());
        spawn_op(self.target.host.clone(), async move {
            let request = bollard::models::NetworkDisconnectRequest {
                container,
                force: Some(false),
            };
            d.disconnect_network(&network, request)
                .await
                .map_err(|e| this.err(e))
        })
    }

    pub fn inspect_network(&self, id: &str) -> EngineTask<R<String>> {
        let (d, this, id) = (self.docker.clone(), self.clone(), id.to_owned());
        spawn_op(self.target.host.clone(), async move {
            let v = d
                .inspect_network(&id, None::<q::InspectNetworkOptions>)
                .await
                .map_err(|e| this.err(e))?;
            serde_json::to_string_pretty(&v).map_err(|e| EngineError::Local(e.to_string()))
        })
    }

    // ---- prune ----

    async fn prune_containers_once(&self) -> R<PruneReport> {
        let r = self
            .docker
            .prune_containers(None::<q::PruneContainersOptions>)
            .await
            .map_err(|e| self.err(e))?;
        Ok(PruneReport {
            deleted: r.containers_deleted.map_or(0, |v| v.len()),
            reclaimed: r.space_reclaimed.unwrap_or(0).max(0) as u64,
        })
    }

    async fn prune_networks_once(&self) -> R<PruneReport> {
        let r = self
            .docker
            .prune_networks(None::<q::PruneNetworksOptions>)
            .await
            .map_err(|e| self.err(e))?;
        Ok(PruneReport {
            deleted: r.networks_deleted.map_or(0, |v| v.len()),
            reclaimed: 0,
        })
    }

    async fn prune_images_once(&self, mode: CleanupImages) -> R<PruneReport> {
        let opts = q::PruneImagesOptions {
            filters: Some(image_prune_filter(mode)),
        };
        let r = self
            .docker
            .prune_images(Some(opts))
            .await
            .map_err(|e| self.err(e))?;
        Ok(PruneReport {
            deleted: r.images_deleted.map_or(0, |v| v.len()),
            reclaimed: r.space_reclaimed.unwrap_or(0).max(0) as u64,
        })
    }

    async fn prune_volumes_once(&self, opts: Option<q::PruneVolumesOptions>) -> R<PruneReport> {
        let r = self
            .docker
            .prune_volumes(opts)
            .await
            .map_err(|e| self.err(e))?;
        Ok(PruneReport {
            deleted: r.volumes_deleted.map_or(0, |v| v.len()),
            reclaimed: r.space_reclaimed.unwrap_or(0).max(0) as u64,
        })
    }

    async fn prune_build_once(&self) -> R<PruneReport> {
        let r = self
            .docker
            .prune_build(None::<q::PruneBuildOptions>)
            .await
            .map_err(|e| self.err(e))?;
        Ok(PruneReport {
            deleted: r.caches_deleted.map_or(0, |v| v.len()),
            reclaimed: r.space_reclaimed.unwrap_or(0).max(0) as u64,
        })
    }

    pub fn prune(&self, kind: PruneKind) -> EngineTask<R<PruneReport>> {
        let this = self.clone();
        spawn_op(self.target.host.clone(), async move {
            match kind {
                PruneKind::Containers => this.prune_containers_once().await,
                PruneKind::Images => this.prune_images_once(CleanupImages::Dangling).await,
                PruneKind::Volumes => this.prune_volumes_once(None).await,
                PruneKind::Networks => this.prune_networks_once().await,
                PruneKind::BuildCache => this.prune_build_once().await,
            }
        })
    }

    /// Runs the selected prune steps in dependency order (`Containers` before
    /// `Networks`, `Images` and `Volumes`) so resources held only by stopped
    /// containers are reclaimed in the same pass.
    pub fn cleanup(&self, scope: CleanupScope) -> EngineTask<R<CleanupReport>> {
        let this = self.clone();
        spawn(async move {
            let volume_opts = match scope.volumes {
                Some(mode) => Some(volume_prune_options(mode, &this.api_version)?),
                None => None,
            };
            let host = this.target.host.clone();
            run_cleanup_steps(scope, |step| {
                let this = this.clone();
                let host = host.clone();
                let volume_opts = volume_opts.clone();
                async move {
                    with_timeout(OP_TIMEOUT, &host, async move {
                        match step {
                            CleanupStep::Containers => this.prune_containers_once().await,
                            CleanupStep::Networks => this.prune_networks_once().await,
                            CleanupStep::Images => {
                                this.prune_images_once(
                                    scope.images.unwrap_or(CleanupImages::Dangling),
                                )
                                .await
                            }
                            CleanupStep::Volumes => {
                                this.prune_volumes_once(volume_opts.unwrap_or(None)).await
                            }
                            CleanupStep::BuildCache => this.prune_build_once().await,
                        }
                    })
                    .await
                }
            })
            .await
        })
    }

    // ---- events ----

    /// The daemon's event stream. Ends (with an error item) when the connection drops.
    pub fn events(&self) -> EngineStream<R<EngineEvent>> {
        let this = self.clone();
        let opts = q::EventsOptions {
            filters: Some(event_filters()),
            ..Default::default()
        };
        let stream = self.docker.events(Some(opts));
        let stream = stream.chain(futures::stream::once(async move {
            // A daemon never ends this stream on its own; reaching the end means the
            // connection closed, which the UI must treat as a disconnect.
            Err(bollard::errors::Error::IOError {
                err: std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "event stream closed",
                ),
            })
        }));
        pump(STREAM_CAPACITY, stream, move |item| {
            Some(item.map_err(|e| this.err(e)).map(convert_event))
        })
    }

    pub(crate) fn docker(&self) -> &bollard::Docker {
        &self.docker
    }
}

/// Splits `name[:tag][@digest]` into the `fromImage` / `tag` pair the API wants.
///
/// An empty name or tag is refused rather than sent: the API reads an empty `tag` as
/// "every tag of this repository", so `nginx:` would pull all of them.
pub(crate) fn split_reference(reference: &str) -> R<(String, String)> {
    let reference = reference.trim();
    let (name, tag) = match reference.split_once('@') {
        Some((name, digest)) => (name, digest),
        None => {
            // A ':' after the last '/' is a tag; one before it belongs to a registry port.
            let last_slash = reference.rfind('/').map_or(0, |i| i + 1);
            match reference[last_slash..].rfind(':') {
                Some(i) => {
                    let at = last_slash + i;
                    (&reference[..at], &reference[at + 1..])
                }
                None => (reference, "latest"),
            }
        }
    };
    if name.is_empty() || tag.is_empty() {
        return Err(EngineError::Local(format!(
            "{reference:?} is not an image reference: expected name[:tag] or name@digest."
        )));
    }
    Ok((name.to_owned(), tag.to_owned()))
}

async fn run_cleanup_steps<F, Fut>(scope: CleanupScope, mut run_step: F) -> R<CleanupReport>
where
    F: FnMut(CleanupStep) -> Fut,
    Fut: Future<Output = R<PruneReport>>,
{
    let mut steps = Vec::with_capacity(5);
    if scope.containers {
        steps.push(CleanupStep::Containers);
    }
    if scope.networks {
        steps.push(CleanupStep::Networks);
    }
    if scope.images.is_some() {
        steps.push(CleanupStep::Images);
    }
    if scope.volumes.is_some() {
        steps.push(CleanupStep::Volumes);
    }
    if scope.build_cache {
        steps.push(CleanupStep::BuildCache);
    }

    let mut report = CleanupReport::default();
    let mut completed = 0usize;
    for step in steps {
        match run_step(step).await {
            Ok(r) => {
                completed += 1;
                report.reclaimed = report.reclaimed.saturating_add(r.reclaimed);
                match step {
                    CleanupStep::Containers => report.containers_deleted = r.deleted,
                    CleanupStep::Networks => report.networks_deleted = r.deleted,
                    CleanupStep::Images => report.images_deleted = r.deleted,
                    CleanupStep::Volumes => report.volumes_deleted = r.deleted,
                    CleanupStep::BuildCache => report.build_cache_deleted = r.deleted,
                }
            }
            Err(e) if completed == 0 || e.is_connection_lost() => return Err(e),
            Err(e) => {
                report.partial_error = Some((step, e.to_string()));
                break;
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::{
        container_mem_usage, convert_container, convert_event, convert_volumes, create_body,
        event_filters, image_prune_filter, local_fs_total_bytes, run_cleanup_steps,
        spawn_op_with_timeout, split_reference, volume_prune_options, volume_sizes_from_items,
    };
    use crate::{
        CleanupImages, CleanupScope, CleanupStep, CleanupVolumes, EngineError, PruneReport,
        PublishedPort, RestartPolicy, RunSpec, VolumeMount,
    };

    #[test]
    fn splits_references() {
        let s = |r| split_reference(r).unwrap();
        assert_eq!(s("nginx"), ("nginx".into(), "latest".into()));
        assert_eq!(s("nginx:1.27"), ("nginx".into(), "1.27".into()));
        assert_eq!(
            s("localhost:5000/app"),
            ("localhost:5000/app".into(), "latest".into())
        );
        assert_eq!(
            s("localhost:5000/app:v2"),
            ("localhost:5000/app".into(), "v2".into())
        );
        assert_eq!(
            s("alpine@sha256:abc"),
            ("alpine".into(), "sha256:abc".into())
        );
        // The digest wins; dockerd drops the tag from fromImage itself.
        assert_eq!(
            s("nginx:1.27@sha256:abc"),
            ("nginx:1.27".into(), "sha256:abc".into())
        );
        // Without a '/', a registry port reads as a tag, as `docker pull` reads it.
        assert_eq!(s("localhost:5000"), ("localhost".into(), "5000".into()));
    }

    #[test]
    fn refuses_references_with_an_empty_name_or_tag() {
        // An empty tag asks the daemon for every tag of the repository.
        for r in [
            "",
            "   ",
            "nginx:",
            "alpine@",
            ":v1",
            "@sha256:abc",
            "localhost:5000/app:",
        ] {
            assert!(
                matches!(split_reference(r), Err(EngineError::Local(_))),
                "{r:?}: {:?}",
                split_reference(r)
            );
        }
    }

    #[test]
    fn pull_of_an_invalid_reference_fails_without_a_request() {
        use futures::StreamExt;

        let target = crate::ConnectTarget {
            // Nothing listens here: a request would fail as Transport, not Local.
            host: "tcp://127.0.0.1:1".into(),
            source: String::new(),
            context: None,
            tls: None,
        };
        let engine = crate::Engine {
            docker: bollard::Docker::connect_with_host(&target.host).unwrap(),
            target: std::sync::Arc::new(target),
            api_version: "1.52".into(),
        };
        let items: Vec<_> = futures::executor::block_on(engine.pull("nginx:").collect());
        assert_eq!(items.len(), 1, "{items:?}");
        assert!(
            matches!(&items[0], Err(EngineError::Local(m)) if m.contains("nginx:")),
            "{items:?}"
        );
    }

    #[test]
    fn volume_sizes_skip_uncomputed_and_malformed_items() {
        let items: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
                {"Name": "data", "UsageData": {"Size": 2048, "RefCount": 1}},
                {"Name": "pending", "UsageData": {"Size": -1, "RefCount": 0}},
                {"Name": "bare"}
            ]"#,
        )
        .unwrap();
        let sizes = volume_sizes_from_items(&items);
        assert_eq!(sizes.len(), 1);
        assert_eq!(sizes.get("data"), Some(&2048));
    }

    #[test]
    fn volumes_are_listed_by_name_whatever_order_the_daemon_uses() {
        let volume = |name: &str| bollard::models::Volume {
            name: name.into(),
            driver: "local".into(),
            mountpoint: format!("/var/lib/docker/volumes/{name}/_data"),
            labels: HashMap::from([("k".to_string(), "v".to_string())]),
            ..Default::default()
        };
        let a = convert_volumes(vec![volume("db"), volume("cache"), volume("app")]);
        let b = convert_volumes(vec![volume("app"), volume("db"), volume("cache")]);
        let names: Vec<&str> = a.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["app", "cache", "db"]);
        assert_eq!(a, b);
        assert_eq!(a[0].driver, "local");
        assert_eq!(a[0].mountpoint, "/var/lib/docker/volumes/app/_data");
        assert_eq!(a[0].labels.get("k").map(String::as_str), Some("v"));
        assert!(convert_volumes(Vec::new()).is_empty());
    }

    #[test]
    fn container_lists_attached_networks_and_volume_mounts() {
        let c: bollard::models::ContainerSummary = serde_json::from_str(
            r#"{
                "Id": "abc", "Names": ["/web"],
                "NetworkSettings": {"Networks": {"front": {}, "back": {}}},
                "Mounts": [
                    {"Type": "volume", "Name": "data", "Destination": "/data"},
                    {"Type": "bind", "Source": "/srv", "Destination": "/srv"}
                ]
            }"#,
        )
        .unwrap();
        let c = convert_container(c);
        assert_eq!(c.networks, ["back", "front"]);
        assert_eq!(c.volumes, ["data"]);
    }

    fn published(ports: &str) -> Vec<String> {
        let c: bollard::models::ContainerSummary =
            serde_json::from_str(&format!(r#"{{"Id": "abc", "Ports": {ports}}}"#)).unwrap();
        convert_container(c)
            .ports
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn dual_stack_ports_collapse_to_ipv4_for_each_protocol() {
        // dockerd lists 53/tcp and 53/udp once per address family.
        let ports = published(
            r#"[
                {"IP": "0.0.0.0", "PrivatePort": 53, "PublicPort": 53, "Type": "tcp"},
                {"IP": "::", "PrivatePort": 53, "PublicPort": 53, "Type": "tcp"},
                {"IP": "0.0.0.0", "PrivatePort": 53, "PublicPort": 53, "Type": "udp"},
                {"IP": "::", "PrivatePort": 53, "PublicPort": 53, "Type": "udp"}
            ]"#,
        );
        assert_eq!(ports, ["0.0.0.0:53→53/tcp", "0.0.0.0:53→53/udp"]);
    }

    #[test]
    fn ports_on_different_ipv4_addresses_are_all_kept() {
        let ports = published(
            r#"[
                {"IP": "127.0.0.1", "PrivatePort": 80, "PublicPort": 8080, "Type": "tcp"},
                {"IP": "192.168.1.5", "PrivatePort": 80, "PublicPort": 8080, "Type": "tcp"}
            ]"#,
        );
        assert_eq!(ports, ["127.0.0.1:8080→80/tcp", "192.168.1.5:8080→80/tcp"]);
    }

    #[test]
    fn ipv6_only_and_unpublished_ports_are_kept() {
        let ports = published(
            r#"[
                {"IP": "::1", "PrivatePort": 80, "PublicPort": 8080, "Type": "tcp"},
                {"PrivatePort": 443, "Type": "tcp"}
            ]"#,
        );
        assert_eq!(ports, ["[::1]:8080→80/tcp", "443/tcp"]);
    }

    #[test]
    fn all_ipv6_stays_when_ipv4_is_bound_to_one_address() {
        // `-p 127.0.0.1:8080:80 -p [::]:8080:80`: reachable from outside over IPv6.
        let ports = published(
            r#"[
                {"IP": "::", "PrivatePort": 80, "PublicPort": 8080, "Type": "tcp"},
                {"IP": "127.0.0.1", "PrivatePort": 80, "PublicPort": 8080, "Type": "tcp"}
            ]"#,
        );
        assert_eq!(ports, ["[::]:8080→80/tcp", "127.0.0.1:8080→80/tcp"]);
    }

    #[test]
    fn event_filters_restrict_daemon_event_types() {
        // P-007: server-side event subscription filters out builder/plugin/daemon noise.
        let filters = event_filters();
        let types = filters.get("type").expect("type filter");
        assert_eq!(types, &["container", "image", "volume", "network"]);
    }

    #[test]
    fn events_carry_the_container_name_and_exit_code() {
        use bollard::models::{EventActor, EventMessage, EventMessageTypeEnum};
        let die = EventMessage {
            typ: Some(EventMessageTypeEnum::CONTAINER),
            action: Some("die".into()),
            actor: Some(EventActor {
                id: Some("abc".into()),
                attributes: Some(HashMap::from([
                    ("name".to_string(), "web".to_string()),
                    ("exitCode".to_string(), "137".to_string()),
                    ("image".to_string(), "nginx".to_string()),
                ])),
            }),
            ..Default::default()
        };
        let ev = convert_event(die);
        assert_eq!(ev.kind, crate::ResourceKind::Container);
        assert_eq!(ev.action, "die");
        assert_eq!(ev.id, "abc");
        assert_eq!(ev.name.as_deref(), Some("web"));
        assert_eq!(ev.exit_code, Some(137));

        // No actor, or an exit code that is not a number: nothing is made up.
        let bare = convert_event(EventMessage {
            typ: Some(EventMessageTypeEnum::DAEMON),
            actor: Some(EventActor {
                id: None,
                attributes: Some(HashMap::from([("exitCode".to_string(), "x".to_string())])),
            }),
            ..Default::default()
        });
        assert_eq!(bare.kind, crate::ResourceKind::Other);
        assert_eq!((bare.name, bare.exit_code), (None, None));
    }

    #[test]
    fn spawn_op_enforces_end_to_end_deadline() {
        // P-010: stalled operations time out cleanly instead of hanging forever.
        let ok_res = futures::executor::block_on(spawn_op_with_timeout(
            Duration::from_millis(200),
            "unix:///test".into(),
            async { Ok::<_, EngineError>(42) },
        ));
        assert_eq!(ok_res, Ok(42));

        let stalled = futures::executor::block_on(spawn_op_with_timeout(
            Duration::from_millis(25),
            "unix:///test".into(),
            futures::future::pending::<Result<(), EngineError>>(),
        ));
        assert!(
            matches!(stalled, Err(EngineError::Timeout(ref msg)) if msg.contains("timed out")),
            "{stalled:?}"
        );
    }

    #[test]
    fn mem_usage_subtracts_cgroup_page_cache() {
        let mem = bollard::models::ContainerMemoryStats {
            usage: Some(500_000_000),
            stats: Some(HashMap::from([("inactive_file".into(), 120_000_000)])),
            ..Default::default()
        };
        assert_eq!(container_mem_usage(&mem), 380_000_000);
    }

    #[cfg(unix)]
    #[test]
    fn local_fs_total_bytes_reads_mounted_filesystem_and_rejects_missing_paths() {
        assert!(local_fs_total_bytes(Some("/")).is_some_and(|n| n > 0));
        assert_eq!(local_fs_total_bytes(None), None);
        assert_eq!(local_fs_total_bytes(Some("")), None);
        assert_eq!(
            local_fs_total_bytes(Some("/nonexistent/docker-stack-statvfs-test")),
            None
        );
    }

    #[test]
    fn create_body_publishes_ports_mounts_named_volumes_and_sets_policy() {
        let spec = RunSpec {
            image: "nginx:1.27".into(),
            name: Some("web".into()),
            cmd: vec!["nginx".into(), "-g".into(), "daemon off;".into()],
            env: vec!["A=1".into()],
            ports: vec![
                PublishedPort {
                    host_ip: None,
                    host_port: Some(8080),
                    container_port: 80,
                    protocol: "tcp".into(),
                },
                PublishedPort {
                    host_ip: Some("127.0.0.1".into()),
                    host_port: Some(8443),
                    container_port: 80,
                    protocol: "tcp".into(),
                },
                PublishedPort {
                    host_ip: None,
                    host_port: None,
                    container_port: 53,
                    protocol: "udp".into(),
                },
            ],
            volumes: vec![VolumeMount {
                volume: "data".into(),
                target: "/data".into(),
                read_only: true,
            }],
            network: Some("backend".into()),
            restart: RestartPolicy::UnlessStopped,
        };
        let v = serde_json::to_value(create_body(&spec)).unwrap();
        assert_eq!(v["Image"], "nginx:1.27");
        assert_eq!(v["Cmd"], serde_json::json!(["nginx", "-g", "daemon off;"]));
        assert_eq!(v["Env"], serde_json::json!(["A=1"]));
        assert_eq!(
            v["ExposedPorts"],
            serde_json::json!({"53/udp": {}, "80/tcp": {}})
        );
        let host = &v["HostConfig"];
        assert_eq!(
            host["PortBindings"]["80/tcp"],
            serde_json::json!([
                {"HostPort": "8080"},
                {"HostIp": "127.0.0.1", "HostPort": "8443"}
            ])
        );
        // An empty host port asks dockerd for a free one.
        assert_eq!(
            host["PortBindings"]["53/udp"],
            serde_json::json!([{"HostPort": ""}])
        );
        assert_eq!(
            host["Mounts"],
            serde_json::json!([
                {"Type": "volume", "Source": "data", "Target": "/data", "ReadOnly": true}
            ])
        );
        assert!(host.get("Binds").is_none());
        assert_eq!(host["NetworkMode"], "backend");
        assert_eq!(host["RestartPolicy"]["Name"], "unless-stopped");
    }

    /// The update plan is built on bollard's own serialization of the inspect data and
    /// read back as a create body: the settings it keeps must survive both ways.
    #[test]
    fn an_update_plan_reads_back_as_a_create_body() {
        let raw = serde_json::json!({
            "Id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "Name": "/web",
            "Image": "sha256:old",
            "State": {"Running": true},
            "Config": {
                "Hostname": "0123456789ab",
                "Image": "nginx:1.27",
                "Env": ["MODE=prod"],
                "ExposedPorts": {"80/tcp": {}},
                "Volumes": {"/cache": {}},
                "Labels": {"team": "web"},
                "StopTimeout": 30
            },
            "HostConfig": {
                "Binds": ["/srv:/srv:ro"],
                "PortBindings": {"80/tcp": [{"HostIp": "", "HostPort": "8080"}]},
                "RestartPolicy": {"Name": "always", "MaximumRetryCount": 0},
                "NetworkMode": "front",
                "Memory": 268435456,
                "LogConfig": {"Type": "json-file", "Config": {"max-size": "10m"}}
            },
            "Mounts": [{"Type": "volume", "Name": "a".repeat(64), "Destination": "/cache", "RW": true}],
            "NetworkSettings": {"Networks": {
                "front": {"Aliases": ["web"], "IPAMConfig": {"IPv4Address": "172.30.0.10"}},
                "back": {"Aliases": ["api"]}
            }}
        });
        let inspect: bollard::models::ContainerInspectResponse =
            serde_json::from_value(raw).unwrap();
        let container = serde_json::to_value(&inspect).unwrap();
        let plan = crate::recreate::plan(&container, None, "nginx:1.27").unwrap();
        let body: bollard::models::ContainerCreateBody = serde_json::from_value(plan.body).unwrap();
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["Image"], "nginx:1.27");
        assert_eq!(v["Env"], serde_json::json!(["MODE=prod"]));
        assert_eq!(v["ExposedPorts"], serde_json::json!({"80/tcp": {}}));
        assert_eq!(v["StopTimeout"], 30);
        assert!(v.get("Hostname").is_none(), "{v}");
        let host = &v["HostConfig"];
        assert_eq!(host["Binds"], serde_json::json!(["/srv:/srv:ro"]));
        assert_eq!(host["Memory"], 268435456);
        assert_eq!(host["RestartPolicy"]["Name"], "always");
        assert_eq!(host["LogConfig"]["Config"]["max-size"], "10m");
        assert_eq!(host["Mounts"][0]["Type"], "volume");
        assert_eq!(host["Mounts"][0]["Source"], "a".repeat(64));
        assert_eq!(
            v["NetworkingConfig"]["EndpointsConfig"]["front"]["IPAMConfig"]["IPv4Address"],
            "172.30.0.10"
        );
        let (network, endpoint) = &plan.networks[0];
        assert_eq!(network, "back");
        let endpoint: bollard::models::EndpointSettings =
            serde_json::from_value(endpoint.clone()).unwrap();
        assert_eq!(endpoint.aliases, Some(vec!["api".to_owned()]));
    }

    #[test]
    fn create_body_leaves_out_what_the_image_decides() {
        let spec = RunSpec {
            image: "alpine".into(),
            ..Default::default()
        };
        let v = serde_json::to_value(create_body(&spec)).unwrap();
        for key in ["Cmd", "Env", "ExposedPorts"] {
            assert!(v.get(key).is_none(), "{key}: {v}");
        }
        let host = &v["HostConfig"];
        for key in ["PortBindings", "Mounts", "NetworkMode"] {
            assert!(host.get(key).is_none(), "{key}: {host}");
        }
        assert_eq!(host["RestartPolicy"]["Name"], "no");
    }

    #[test]
    fn cleanup_filters_match_docker_api_modes() {
        assert_eq!(
            image_prune_filter(CleanupImages::Dangling).get("dangling"),
            Some(&vec!["true".to_owned()])
        );
        assert_eq!(
            image_prune_filter(CleanupImages::AllUnused).get("dangling"),
            Some(&vec!["false".to_owned()])
        );

        assert!(volume_prune_options(CleanupVolumes::Anonymous, "1.41").is_err());
        assert!(
            volume_prune_options(CleanupVolumes::AllUnused, "1.41")
                .unwrap()
                .is_none()
        );
        assert!(
            volume_prune_options(CleanupVolumes::Anonymous, "1.42")
                .unwrap()
                .is_none()
        );
        let all = volume_prune_options(CleanupVolumes::AllUnused, "1.52")
            .unwrap()
            .expect("filters on >= 1.42");
        assert_eq!(
            all.filters.as_ref().and_then(|f| f.get("all")),
            Some(&vec!["true".to_owned()])
        );
    }

    #[tokio::test]
    async fn cleanup_runs_in_dependency_order_and_preserves_partial_progress() {
        let full = CleanupScope {
            containers: true,
            networks: true,
            images: Some(CleanupImages::AllUnused),
            volumes: Some(CleanupVolumes::Anonymous),
            build_cache: true,
        };
        let mut order = Vec::new();
        let ok = run_cleanup_steps(full, |step| {
            order.push(step);
            async move {
                Ok(PruneReport {
                    deleted: 1,
                    reclaimed: 100,
                })
            }
        })
        .await
        .unwrap();
        assert_eq!(
            order,
            vec![
                CleanupStep::Containers,
                CleanupStep::Networks,
                CleanupStep::Images,
                CleanupStep::Volumes,
                CleanupStep::BuildCache,
            ]
        );
        assert_eq!(ok.total_deleted(), 5);
        assert_eq!(ok.reclaimed, 500);
        assert_eq!(ok.partial_error, None);

        // Non-connection error after step 1 succeeded keeps step 1's counts and stops.
        let mut ran = Vec::new();
        let partial = run_cleanup_steps(full, |step| {
            ran.push(step);
            async move {
                if step == CleanupStep::Images {
                    Err(EngineError::Daemon {
                        status: 500,
                        message: "layer busy".into(),
                    })
                } else {
                    Ok(PruneReport {
                        deleted: 2,
                        reclaimed: 50,
                    })
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            ran,
            vec![
                CleanupStep::Containers,
                CleanupStep::Networks,
                CleanupStep::Images
            ]
        );
        assert_eq!(partial.containers_deleted, 2);
        assert_eq!(partial.networks_deleted, 2);
        assert_eq!(partial.images_deleted, 0);
        assert_eq!(partial.reclaimed, 100);
        assert!(matches!(
            partial.partial_error,
            Some((CleanupStep::Images, _))
        ));

        // First-step failure surfaces as Err directly.
        let err = run_cleanup_steps(full, |_| async {
            Err(EngineError::Conflict("prune already running".into()))
        })
        .await
        .unwrap_err();
        assert!(matches!(err, EngineError::Conflict(_)));

        // Connection-lost mid-cleanup surfaces as Err so reconnect triggers.
        let lost = run_cleanup_steps(full, |step| async move {
            if step == CleanupStep::Networks {
                Err(EngineError::Transport("broken pipe".into()))
            } else {
                Ok(PruneReport {
                    deleted: 1,
                    reclaimed: 10,
                })
            }
        })
        .await
        .unwrap_err();
        assert!(lost.is_connection_lost());

        // Legacy API < 1.42 refuses anonymous-only volume cleanup before sending any request.
        let target = crate::ConnectTarget {
            host: "tcp://127.0.0.1:1".into(),
            source: String::new(),
            context: None,
            tls: None,
        };
        let legacy = crate::Engine {
            docker: bollard::Docker::connect_with_host(&target.host).unwrap(),
            target: std::sync::Arc::new(target),
            api_version: "1.41".into(),
        };
        let refused = legacy
            .cleanup(full)
            .await
            .expect_err("legacy daemon must refuse anonymous-only volume prune");
        assert!(matches!(refused, EngineError::Local(_)), "{refused:?}");
    }
}
