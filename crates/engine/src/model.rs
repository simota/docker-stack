//! Domain types handed to the UI. Built from bollard models in `ops.rs` only.

use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineInfo {
    pub version: String,
    pub api_version: String,
    pub os: String,
    pub arch: String,
    pub host: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContainerState {
    Created,
    Running,
    Paused,
    Restarting,
    Exited,
    Removing,
    Dead,
    Unknown,
}

impl ContainerState {
    pub fn is_running(self) -> bool {
        matches!(self, ContainerState::Running | ContainerState::Restarting)
    }
    pub fn label(self) -> &'static str {
        match self {
            ContainerState::Created => "created",
            ContainerState::Running => "running",
            ContainerState::Paused => "paused",
            ContainerState::Restarting => "restarting",
            ContainerState::Exited => "exited",
            ContainerState::Removing => "removing",
            ContainerState::Dead => "dead",
            ContainerState::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortMapping {
    pub ip: Option<String>,
    pub private_port: u16,
    pub public_port: Option<u16>,
    pub protocol: String,
}

impl std::fmt::Display for PortMapping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.public_port {
            Some(p) => {
                let ip = self.ip.as_deref().unwrap_or("0.0.0.0");
                // IPv6 in brackets, as `docker ps` shows it: `[::]:8080`, not `:::8080`.
                if ip.contains(':') {
                    write!(f, "[{ip}]:{p}→{}/{}", self.private_port, self.protocol)
                } else {
                    write!(f, "{ip}:{p}→{}/{}", self.private_port, self.protocol)
                }
            }
            None => write!(f, "{}/{}", self.private_port, self.protocol),
        }
    }
}

/// Compose metadata read from the `com.docker.compose.*` labels.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ComposeLabels {
    pub project: String,
    pub service: String,
    pub working_dir: Option<String>,
    pub config_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerSummary {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: ContainerState,
    pub status: String,
    pub created: i64,
    pub ports: Vec<PortMapping>,
    pub compose: Option<ComposeLabels>,
    /// Names of the networks the container is attached to.
    pub networks: Vec<String>,
    /// Names of the named/anonymous volumes mounted into the container.
    pub volumes: Vec<String>,
}

impl ContainerSummary {
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(12)]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSummary {
    pub id: String,
    pub tags: Vec<String>,
    pub size: i64,
    pub created: i64,
    pub containers: i64,
}

impl ImageSummary {
    pub fn short_id(&self) -> &str {
        let id = self.id.strip_prefix("sha256:").unwrap_or(&self.id);
        &id[..id.len().min(12)]
    }
    pub fn is_dangling(&self) -> bool {
        self.tags.is_empty() || self.tags.iter().all(|t| t == "<none>:<none>")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeSummary {
    pub name: String,
    pub driver: String,
    pub mountpoint: String,
    pub created_at: Option<String>,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkSummary {
    pub id: String,
    pub name: String,
    pub driver: String,
    pub scope: String,
    pub internal: bool,
}

impl NetworkSummary {
    /// The networks dockerd creates itself and refuses to delete.
    pub fn is_builtin(&self) -> bool {
        matches!(self.name.as_str(), "bridge" | "host" | "none")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerAction {
    Start,
    Stop,
    Restart,
    Kill,
    Pause,
    Unpause,
    Remove { force: bool, volumes: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStream {
    Stdout,
    Stderr,
    Console,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogChunk {
    pub stream: LogStream,
    /// Raw text; may hold several lines or a partial line.
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StatsSample {
    pub cpu_percent: f64,
    pub mem_usage: u64,
    pub mem_limit: u64,
    pub net_rx: u64,
    pub net_tx: u64,
    pub pids: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullProgress {
    pub layer: Option<String>,
    pub status: String,
    pub current: Option<i64>,
    pub total: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceKind {
    Container,
    Image,
    Volume,
    Network,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineEvent {
    pub kind: ResourceKind,
    pub action: String,
    pub id: String,
    /// The object's name, when the daemon sends it (a container's name has no `/`).
    pub name: Option<String>,
    /// A container's exit code, on its `die` event.
    pub exit_code: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneKind {
    Containers,
    /// Dangling images only.
    Images,
    /// Anonymous, unused volumes (dockerd's default prune scope).
    Volumes,
    Networks,
    /// Unused BuildKit cache.
    BuildCache,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PruneReport {
    pub deleted: usize,
    pub reclaimed: u64,
}

/// API 1.42 (Docker 23) made `volume prune` remove only anonymous volumes by default
/// and added the `all=true` filter for named volumes.
pub fn prune_removes_named_volumes(api_version: Option<&str>) -> bool {
    let Some(v) = api_version else { return false };
    let mut parts = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    major < 1 || (major == 1 && minor < 42)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupImages {
    Dangling,
    AllUnused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupVolumes {
    /// Anonymous unused volumes only (requires Docker Engine API >= 1.42).
    Anonymous,
    /// All unused volumes, including named volumes.
    AllUnused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CleanupScope {
    pub containers: bool,
    pub networks: bool,
    pub images: Option<CleanupImages>,
    pub volumes: Option<CleanupVolumes>,
    pub build_cache: bool,
}

impl CleanupScope {
    pub fn is_empty(&self) -> bool {
        !self.containers
            && !self.networks
            && self.images.is_none()
            && self.volumes.is_none()
            && !self.build_cache
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupStep {
    Containers,
    Networks,
    Images,
    Volumes,
    BuildCache,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CleanupReport {
    pub containers_deleted: usize,
    pub networks_deleted: usize,
    pub images_deleted: usize,
    pub volumes_deleted: usize,
    pub build_cache_deleted: usize,
    pub reclaimed: u64,
    /// Set when a step fails after at least one earlier step succeeded.
    pub partial_error: Option<(CleanupStep, String)>,
}

impl CleanupReport {
    pub fn total_deleted(&self) -> usize {
        self.containers_deleted
            + self.networks_deleted
            + self.images_deleted
            + self.volumes_deleted
            + self.build_cache_deleted
    }
}

/// When dockerd restarts a container on its own (`docker run --restart`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestartPolicy {
    #[default]
    No,
    Always,
    UnlessStopped,
    OnFailure,
}

/// A published port (`docker run -p [ip:][host:]container[/proto]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedPort {
    pub host_ip: Option<String>,
    /// `None` lets dockerd pick a free host port.
    pub host_port: Option<u16>,
    pub container_port: u16,
    /// `tcp`, `udp` or `sctp`.
    pub protocol: String,
}

/// A named volume mounted into a container. Host paths (bind mounts) are not offered:
/// the UI may not hand host paths to the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeMount {
    pub volume: String,
    /// Absolute path inside the container.
    pub target: String,
    pub read_only: bool,
}

/// What `docker run` creates: an already validated description of a new container.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunSpec {
    pub image: String,
    pub name: Option<String>,
    /// Replaces the image's `CMD` when not empty.
    pub cmd: Vec<String>,
    /// `KEY=VALUE` entries.
    pub env: Vec<String>,
    pub ports: Vec<PublishedPort>,
    pub volumes: Vec<VolumeMount>,
    /// A network to attach to instead of the default bridge.
    pub network: Option<String>,
    pub restart: RestartPolicy,
}

/// The settings of an existing container, as a [`RunSpec`] for another one like it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContainerTemplate {
    /// `name` is the original container's.
    pub spec: RunSpec,
    /// What the container has that a [`RunSpec`] cannot carry.
    pub skipped: Vec<SkippedSetting>,
}

/// How a container's image compares with the newest one for its tag
/// ([`crate::Engine::check_updates`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageFreshness {
    /// The registry's image for the tag is the one the container runs.
    Current,
    /// The registry has a newer image for the tag.
    Available,
    /// A newer image for the tag is already here (pulled or built since).
    Pulled,
    /// Could not tell (no tag, a local-only image, a registry that refused).
    Unknown(String),
}

/// One container's result of [`crate::Engine::check_updates`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheck {
    pub container: String,
    pub freshness: ImageFreshness,
}

/// What [`crate::Engine::update_container`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// The container's image reference still names the image it runs.
    UpToDate,
    /// The container was replaced by `id`, made from the image the reference names now.
    Updated {
        id: String,
        /// The old container's name when it could not be removed afterwards.
        leftover: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkippedSetting {
    /// A bind mount, tmpfs or anonymous volume (`source` is the host path, if any).
    Mount {
        kind: String,
        source: Option<String>,
        target: String,
    },
    /// An entrypoint other than the image's.
    Entrypoint(Vec<String>),
    /// A network mode that is not a network name (`container:<id>`).
    NetworkMode(String),
    /// Any other setting the Run form has no field for, as its `docker run` flag
    /// (`--memory 512m`, `--cap-add NET_ADMIN`, `--label team=web`).
    Option(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SystemCapacity {
    pub cpu_cores: u32,
    pub mem_total: u64,
    pub disk_total: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerCpuReading {
    pub total_usage: u64,
    pub system_usage: u64,
    pub online_cpus: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContainerUsageSample {
    pub cpu_readings: HashMap<String, ContainerCpuReading>,
    pub mem_usage: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiskUsage {
    pub images: u64,
    pub containers: u64,
    pub volumes: u64,
    pub build_cache: u64,
    /// Per-volume size in bytes, when the daemon reports it (`/system/df?verbose=1`).
    pub volume_sizes: BTreeMap<String, u64>,
}

impl DiskUsage {
    pub fn total_used(&self) -> u64 {
        self.images
            .saturating_add(self.containers)
            .saturating_add(self.volumes)
            .saturating_add(self.build_cache)
    }
}

/// Pure CPU percentage as `docker stats` computes it.
pub fn cpu_percent(
    total: u64,
    pre_total: u64,
    system: u64,
    pre_system: u64,
    online_cpus: u32,
) -> f64 {
    let cpu_delta = total.saturating_sub(pre_total) as f64;
    let sys_delta = system.saturating_sub(pre_system) as f64;
    if cpu_delta <= 0.0 || sys_delta <= 0.0 {
        return 0.0;
    }
    cpu_delta / sys_delta * online_cpus.max(1) as f64 * 100.0
}

/// Aggregates CPU usage in cores across running containers from two one-shot readings.
/// Returns `Some(0.0)` when `curr` is empty, or `None` when `curr` shares no container with `prev`.
pub fn aggregate_cpu_cores(
    prev: &HashMap<String, ContainerCpuReading>,
    curr: &HashMap<String, ContainerCpuReading>,
    fallback_cpus: u32,
) -> Option<f64> {
    if curr.is_empty() {
        return Some(0.0);
    }
    let mut sum_cores = 0.0;
    let mut matched = false;
    for (id, now) in curr {
        if let Some(before) = prev.get(id) {
            matched = true;
            let cpus = now
                .online_cpus
                .or(before.online_cpus)
                .unwrap_or(fallback_cpus)
                .max(1);
            sum_cores += cpu_percent(
                now.total_usage,
                before.total_usage,
                now.system_usage,
                before.system_usage,
                cpus,
            ) / 100.0;
        }
    }
    matched.then_some(sum_cores)
}

pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_percent_matches_docker_formula() {
        // 2 CPUs, container used 1/4 of the system delta → 50%.
        assert_eq!(cpu_percent(150, 100, 1200, 1000, 2), 50.0);
        assert_eq!(cpu_percent(100, 100, 1200, 1000, 2), 0.0);
        assert_eq!(cpu_percent(150, 100, 1000, 1000, 2), 0.0);
    }

    #[test]
    fn bytes_are_decimal_like_docker() {
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_500), "1.5 KB");
        assert_eq!(format_bytes(187_000_000), "187.0 MB");
    }

    #[test]
    fn port_display() {
        let p = PortMapping {
            ip: Some("0.0.0.0".into()),
            private_port: 80,
            public_port: Some(8080),
            protocol: "tcp".into(),
        };
        assert_eq!(p.to_string(), "0.0.0.0:8080→80/tcp");
        let p = PortMapping {
            ip: Some("::1".into()),
            ..p
        };
        assert_eq!(p.to_string(), "[::1]:8080→80/tcp");
        let p = PortMapping {
            ip: None,
            private_port: 5432,
            public_port: None,
            protocol: "tcp".into(),
        };
        assert_eq!(p.to_string(), "5432/tcp");
    }

    #[test]
    fn aggregate_cpu_cores_handles_empty_first_and_followup_samples() {
        let empty = HashMap::new();
        assert_eq!(aggregate_cpu_cores(&empty, &empty, 4), Some(0.0));

        let t0 = HashMap::from([
            (
                "c1".into(),
                ContainerCpuReading {
                    total_usage: 100,
                    system_usage: 1_000,
                    online_cpus: Some(4),
                },
            ),
            (
                "c2".into(),
                ContainerCpuReading {
                    total_usage: 200,
                    system_usage: 1_000,
                    online_cpus: None,
                },
            ),
        ]);
        assert_eq!(aggregate_cpu_cores(&empty, &t0, 4), None);

        let t1 = HashMap::from([
            (
                "c1".into(),
                ContainerCpuReading {
                    total_usage: 200,
                    system_usage: 1_400,
                    online_cpus: Some(4),
                },
            ),
            (
                "c2".into(),
                ContainerCpuReading {
                    total_usage: 300,
                    system_usage: 1_400,
                    online_cpus: None,
                },
            ),
        ]);
        // Each container used 100/400 of a 4-CPU host = 1.0 core each -> 2.0 cores total.
        assert_eq!(aggregate_cpu_cores(&t0, &t1, 4), Some(2.0));
    }

    #[test]
    fn disk_usage_sums_all_categories() {
        let d = DiskUsage {
            images: 10,
            containers: 20,
            volumes: 30,
            build_cache: 40,
            ..Default::default()
        };
        assert_eq!(d.total_used(), 100);
    }

    #[test]
    fn cleanup_scope_and_api_version_boundaries() {
        assert!(!prune_removes_named_volumes(None));
        assert!(prune_removes_named_volumes(Some("")));
        assert!(prune_removes_named_volumes(Some("1.41")));
        assert!(!prune_removes_named_volumes(Some("1.42")));
        assert!(!prune_removes_named_volumes(Some("1.52")));
        assert!(!prune_removes_named_volumes(Some("2.0")));

        let mut scope = CleanupScope::default();
        assert!(scope.is_empty());
        scope.images = Some(CleanupImages::Dangling);
        assert!(!scope.is_empty());

        let report = CleanupReport {
            containers_deleted: 2,
            networks_deleted: 1,
            images_deleted: 3,
            volumes_deleted: 4,
            build_cache_deleted: 5,
            reclaimed: 1024,
            partial_error: None,
        };
        assert_eq!(report.total_deleted(), 15);
    }
}
