//! Types the backend hands to the UI; engine types never cross.
//! Numbers that may exceed 32 bits (sizes, timestamps) are `f64`: exact up to 2^53.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiErrorKind {
    PermissionDenied,
    SocketMissing,
    NotFound,
    Conflict,
    Daemon,
    Transport,
    Timeout,
    Local,
}

/// An error as the UI sees it: `kind` drives behaviour, `message` is for people.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiError {
    pub kind: UiErrorKind,
    pub message: String,
}

impl From<&engine::EngineError> for UiError {
    fn from(e: &engine::EngineError) -> Self {
        use engine::EngineError as E;
        let kind = match e {
            E::SocketMissing(_) => UiErrorKind::SocketMissing,
            E::PermissionDenied(_) => UiErrorKind::PermissionDenied,
            E::NotFound(_) => UiErrorKind::NotFound,
            E::Conflict(_) => UiErrorKind::Conflict,
            E::Daemon { .. } => UiErrorKind::Daemon,
            E::Transport(_) => UiErrorKind::Transport,
            E::Timeout(_) => UiErrorKind::Timeout,
            E::Local(_) => UiErrorKind::Local,
            E::NotRestored { error, .. } => UiError::from(&**error).kind,
        };
        UiError {
            kind,
            message: e.to_string(),
        }
    }
}

impl From<engine::EngineError> for UiError {
    fn from(e: engine::EngineError) -> Self {
        UiError::from(&e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineInfo {
    pub version: String,
    pub api_version: String,
    pub os: String,
    pub arch: String,
}

impl From<&engine::EngineInfo> for EngineInfo {
    fn from(i: &engine::EngineInfo) -> Self {
        EngineInfo {
            version: i.version.clone(),
            api_version: i.api_version.clone(),
            os: i.os.clone(),
            arch: i.arch.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub source: String,
    pub context: Option<String>,
}

impl From<&engine::ConnectTarget> for Target {
    fn from(t: &engine::ConnectTarget) -> Self {
        Target {
            host: t.host.clone(),
            source: t.source.clone(),
            context: t.context.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Connected {
        info: EngineInfo,
    },
    /// Was connected and lost it; lists are kept (stale). Retrying automatically.
    Reconnecting {
        error: UiError,
        retry_in: u32,
    },
    /// Never connected to this target; retrying automatically after `retry_in` seconds.
    Unreachable {
        error: UiError,
        retry_in: u32,
    },
    /// The socket may not be opened by this user; retried only on request.
    PermissionDenied {
        error: UiError,
    },
}

// ---- resources ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

impl From<engine::ContainerState> for ContainerState {
    fn from(s: engine::ContainerState) -> Self {
        use engine::ContainerState as S;
        match s {
            S::Created => Self::Created,
            S::Running => Self::Running,
            S::Paused => Self::Paused,
            S::Restarting => Self::Restarting,
            S::Exited => Self::Exited,
            S::Removing => Self::Removing,
            S::Dead => Self::Dead,
            S::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeLabels {
    pub project: String,
    pub service: String,
}

impl From<&engine::ComposeLabels> for ComposeLabels {
    fn from(l: &engine::ComposeLabels) -> Self {
        ComposeLabels {
            project: l.project.clone(),
            service: l.service.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: ContainerState,
    pub status: String,
    pub created: f64,
    /// Published ports, formatted (`0.0.0.0:8080→80/tcp`).
    pub ports: Vec<String>,
    pub compose: Option<ComposeLabels>,
    /// Attached network names.
    pub networks: Vec<String>,
    /// Mounted volume names.
    pub volumes: Vec<String>,
}

impl From<&engine::ContainerSummary> for Container {
    fn from(c: &engine::ContainerSummary) -> Self {
        Container {
            id: c.id.clone(),
            name: c.name.clone(),
            image: c.image.clone(),
            state: c.state.into(),
            status: c.status.clone(),
            created: c.created as f64,
            ports: c
                .ports
                .iter()
                .filter(|p| p.public_port.is_some())
                .map(|p| p.to_string())
                .collect(),
            compose: c.compose.as_ref().map(Into::into),
            networks: c.networks.clone(),
            volumes: c.volumes.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub id: String,
    pub short_id: String,
    pub tags: Vec<String>,
    pub size: f64,
    pub created: f64,
    /// Containers using the image; -1 when the daemon did not compute it.
    pub containers: f64,
    pub dangling: bool,
}

impl From<&engine::ImageSummary> for Image {
    fn from(i: &engine::ImageSummary) -> Self {
        Image {
            id: i.id.clone(),
            short_id: i.short_id().to_owned(),
            tags: i.tags.clone(),
            size: i.size as f64,
            created: i.created as f64,
            containers: i.containers as f64,
            dangling: i.is_dangling(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Volume {
    pub name: String,
    pub driver: String,
    pub mountpoint: String,
    pub created_at: Option<String>,
    pub compose_project: Option<String>,
    /// Created without a name (`docker run -v /path`); what `prune` removes by default.
    pub anonymous: bool,
    /// Bytes on disk, when the daemon reports it.
    pub size: Option<f64>,
}

impl Volume {
    pub fn new(v: &engine::VolumeSummary, size: Option<u64>) -> Self {
        Volume {
            name: v.name.clone(),
            driver: v.driver.clone(),
            mountpoint: v.mountpoint.clone(),
            created_at: v.created_at.clone(),
            compose_project: v.labels.get("com.docker.compose.project").cloned(),
            anonymous: v.labels.contains_key("com.docker.volume.anonymous"),
            size: size.map(|s| s as f64),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub id: String,
    pub name: String,
    pub driver: String,
    pub scope: String,
    pub internal: bool,
    pub builtin: bool,
}

impl From<&engine::NetworkSummary> for Network {
    fn from(n: &engine::NetworkSummary) -> Self {
        Network {
            id: n.id.clone(),
            name: n.name.clone(),
            driver: n.driver.clone(),
            scope: n.scope.clone(),
            internal: n.internal,
            builtin: n.is_builtin(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pull {
    pub reference: String,
    pub status: String,
    /// 0..1 when layer sizes are known.
    pub fraction: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunResult {
    Running,
    Ok,
    Failed { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeRun {
    pub project: String,
    pub command: String,
    pub lines: Vec<String>,
    pub result: RunResult,
}

/// Starting the local daemon from the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonStatus {
    /// What the app would start for the current target ("Docker Desktop", ...), if it can.
    pub startable: Option<String>,
    /// Set while a start is in progress.
    pub starting: Option<String>,
    /// How long a start waits for the socket.
    pub ready_timeout_secs: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DiskBreakdown {
    pub images: f64,
    pub containers: f64,
    pub volumes: f64,
    pub build_cache: f64,
}

impl From<&engine::DiskUsage> for DiskBreakdown {
    fn from(d: &engine::DiskUsage) -> Self {
        Self {
            images: d.images as f64,
            containers: d.containers as f64,
            volumes: d.volumes as f64,
            build_cache: d.build_cache as f64,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SystemUsage {
    /// CPU cores used across running containers (`None` until two samples establish a delta).
    pub cpu_used_cores: Option<f64>,
    /// Host CPU cores available to Docker (`NCPU` from `/info`).
    pub cpu_cores: Option<u32>,
    /// Memory used across running containers in bytes (page cache subtracted, like `docker stats`).
    pub mem_usage: Option<f64>,
    /// Total memory available to Docker in bytes (`MemTotal` from `/info`).
    pub mem_total: Option<f64>,
    /// Docker disk usage in bytes (`Images + Containers + Volumes + BuildCache` from `/system/df`).
    pub disk_usage: Option<f64>,
    /// Total filesystem capacity in bytes for `DockerRootDir` (local daemon only).
    pub disk_total: Option<f64>,
    pub disk_breakdown: Option<DiskBreakdown>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// Increases on every change; the UI keeps the snapshot with the highest revision,
    /// since an invoke reply and an event may arrive in either order.
    pub revision: u32,
    pub conn: ConnState,
    pub target: Target,
    pub containers: Vec<Container>,
    pub images: Vec<Image>,
    pub volumes: Vec<Volume>,
    pub networks: Vec<Network>,
    /// Ids (or `compose:<project>`, `prune`) with an operation in flight.
    pub pending: Vec<String>,
    pub pulls: Vec<Pull>,
    pub compose_runs: Vec<ComposeRun>,
    pub compose_available: Option<bool>,
    pub usage: SystemUsage,
    pub daemon: DaemonStatus,
}

/// Incremental state delta: unchanged resource slices are `None` so high-rate
/// progress ticks (`pull`, `compose`) do not clone or serialize unaffected lists.
#[derive(Debug, Clone, PartialEq)]
pub struct StatePatch {
    pub revision: u32,
    pub conn: ConnState,
    pub target: Target,
    pub containers: Option<Vec<Container>>,
    pub images: Option<Vec<Image>>,
    pub volumes: Option<Vec<Volume>>,
    pub networks: Option<Vec<Network>>,
    pub pending: Option<Vec<String>>,
    pub pulls: Option<Vec<Pull>>,
    pub compose_runs: Option<Vec<ComposeRun>>,
    pub compose_available: Option<bool>,
    pub usage: Option<SystemUsage>,
    pub daemon: DaemonStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Success,
    Error,
}

/// An operation the backend runs for the UI, as a [`Notice`] names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Container(ContainerAction),
    Update,
    ConnectNetwork(String),
    DisconnectNetwork(String),
    /// Run a container from this image.
    Run(String),
    RemoveImage,
    RemoveVolume(String),
    RemoveNetwork,
    CreateVolume(String),
    CreateNetwork(String),
    Prune(PruneKind),
    Cleanup,
}

/// A toast for the user, worded by the UI in its language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// `op` failed with the engine's `error`.
    Failed {
        op: Op,
        error: String,
    },
    /// A container from the Run form started (`id` is its short id).
    RunStarted {
        label: String,
        id: String,
    },
    /// The daemon behind this label started.
    DaemonStarted(String),
    Pulled(String),
    PullFailed {
        reference: String,
        error: String,
    },
    Pruned {
        kind: PruneKind,
        deleted: usize,
        reclaimed: u64,
    },
    /// `docker compose <command>` finished for `project`.
    ComposeDone {
        command: String,
        project: String,
    },
    /// An error whose message is shown as it is (the engine's, or a refused request's).
    Error(String),
}

impl Notice {
    pub fn kind(&self) -> NoticeKind {
        match self {
            Notice::RunStarted { .. }
            | Notice::DaemonStarted(_)
            | Notice::Pulled(_)
            | Notice::Pruned { .. }
            | Notice::ComposeDone { .. } => NoticeKind::Success,
            Notice::Failed { .. } | Notice::PullFailed { .. } | Notice::Error(_) => {
                NoticeKind::Error
            }
        }
    }
}

// ---- command arguments ----

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

impl From<ContainerAction> for engine::ContainerAction {
    fn from(a: ContainerAction) -> Self {
        use engine::ContainerAction as A;
        match a {
            ContainerAction::Start => A::Start,
            ContainerAction::Stop => A::Stop,
            ContainerAction::Restart => A::Restart,
            ContainerAction::Kill => A::Kill,
            ContainerAction::Pause => A::Pause,
            ContainerAction::Unpause => A::Unpause,
            ContainerAction::Remove { force, volumes } => A::Remove { force, volumes },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneKind {
    Containers,
    Images,
    Volumes,
    Networks,
    BuildCache,
}

impl From<PruneKind> for engine::PruneKind {
    fn from(k: PruneKind) -> Self {
        match k {
            PruneKind::BuildCache => engine::PruneKind::BuildCache,
            PruneKind::Containers => engine::PruneKind::Containers,
            PruneKind::Images => engine::PruneKind::Images,
            PruneKind::Volumes => engine::PruneKind::Volumes,
            PruneKind::Networks => engine::PruneKind::Networks,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupImages {
    Dangling,
    AllUnused,
}

impl From<CleanupImages> for engine::CleanupImages {
    fn from(m: CleanupImages) -> Self {
        match m {
            CleanupImages::Dangling => engine::CleanupImages::Dangling,
            CleanupImages::AllUnused => engine::CleanupImages::AllUnused,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupVolumes {
    Anonymous,
    AllUnused,
}

impl From<CleanupVolumes> for engine::CleanupVolumes {
    fn from(m: CleanupVolumes) -> Self {
        match m {
            CleanupVolumes::Anonymous => engine::CleanupVolumes::Anonymous,
            CleanupVolumes::AllUnused => engine::CleanupVolumes::AllUnused,
        }
    }
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

impl From<CleanupScope> for engine::CleanupScope {
    fn from(s: CleanupScope) -> Self {
        engine::CleanupScope {
            containers: s.containers,
            networks: s.networks,
            images: s.images.map(Into::into),
            volumes: s.volumes.map(Into::into),
            build_cache: s.build_cache,
        }
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

impl From<engine::CleanupStep> for CleanupStep {
    fn from(s: engine::CleanupStep) -> Self {
        match s {
            engine::CleanupStep::Containers => CleanupStep::Containers,
            engine::CleanupStep::Networks => CleanupStep::Networks,
            engine::CleanupStep::Images => CleanupStep::Images,
            engine::CleanupStep::Volumes => CleanupStep::Volumes,
            engine::CleanupStep::BuildCache => CleanupStep::BuildCache,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CleanupReport {
    pub containers_deleted: usize,
    pub networks_deleted: usize,
    pub images_deleted: usize,
    pub volumes_deleted: usize,
    pub build_cache_deleted: usize,
    pub reclaimed: f64,
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

impl From<engine::CleanupReport> for CleanupReport {
    fn from(r: engine::CleanupReport) -> Self {
        CleanupReport {
            containers_deleted: r.containers_deleted,
            networks_deleted: r.networks_deleted,
            images_deleted: r.images_deleted,
            volumes_deleted: r.volumes_deleted,
            build_cache_deleted: r.build_cache_deleted,
            reclaimed: r.reclaimed as f64,
            partial_error: r.partial_error.map(|(step, msg)| (step.into(), msg)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectKind {
    Container,
    Image,
    Volume,
    Network,
}

/// `docker run --restart`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RestartChoice {
    #[default]
    No,
    Always,
    UnlessStopped,
    OnFailure,
}

impl RestartChoice {
    pub const ALL: [RestartChoice; 4] = [
        RestartChoice::No,
        RestartChoice::Always,
        RestartChoice::UnlessStopped,
        RestartChoice::OnFailure,
    ];

    /// The CLI's name for it.
    pub fn label(self) -> &'static str {
        match self {
            RestartChoice::No => "no",
            RestartChoice::Always => "always",
            RestartChoice::UnlessStopped => "unless-stopped",
            RestartChoice::OnFailure => "on-failure",
        }
    }
}

/// The Run form as typed: `run_spec::parse` checks it and builds the request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunForm {
    pub image: String,
    pub name: String,
    /// Replaces the image's command; split like a shell does.
    pub command: String,
    /// `-p` entries separated by commas, spaces or new lines.
    pub ports: String,
    /// `KEY=VALUE` lines.
    pub env: String,
    /// `volume:/path[:ro]` entries.
    pub volumes: String,
    pub network: String,
    pub restart: RestartChoice,
}

/// The Exec tab's fields as typed; an empty one keeps the default (bash if the image
/// has it, else sh; the container's user and working directory).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecShell {
    /// Split like a shell does.
    pub command: String,
    pub user: String,
    pub working_dir: String,
}

/// A Run form filled in from an existing container ("Duplicate").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunTemplate {
    /// The container it was made from.
    pub source: String,
    pub form: RunForm,
    /// What the container has that the form cannot carry over.
    pub skipped: Vec<SkippedSetting>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkippedSetting {
    /// A bind mount, tmpfs or anonymous volume, shown as `kind source → target`.
    Mount {
        kind: String,
        source: Option<String>,
        target: String,
    },
    Entrypoint(String),
    NetworkMode(String),
    /// An environment variable (by name) whose value cannot be one line of the form.
    Env(String),
    /// Any other setting, as its `docker run` flag.
    Option(String),
}

impl From<engine::SkippedSetting> for SkippedSetting {
    fn from(s: engine::SkippedSetting) -> Self {
        match s {
            engine::SkippedSetting::Mount {
                kind,
                source,
                target,
            } => SkippedSetting::Mount {
                kind,
                source,
                target,
            },
            engine::SkippedSetting::Entrypoint(words) => {
                SkippedSetting::Entrypoint(crate::run_spec::quote_command(&words))
            }
            engine::SkippedSetting::NetworkMode(mode) => SkippedSetting::NetworkMode(mode),
            engine::SkippedSetting::Option(flag) => SkippedSetting::Option(flag),
        }
    }
}

/// How a container's image compares with the newest one for its tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    Current,
    /// The registry has a newer image.
    Available,
    /// A newer image is already here; Update only replaces the container.
    Pulled,
    /// Could not tell, and why.
    Unknown(String),
}

impl From<engine::ImageFreshness> for Freshness {
    fn from(f: engine::ImageFreshness) -> Self {
        use engine::ImageFreshness as F;
        match f {
            F::Current => Self::Current,
            F::Available => Self::Available,
            F::Pulled => Self::Pulled,
            F::Unknown(why) => Self::Unknown(why),
        }
    }
}

/// One container's result of a check for newer images.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheck {
    pub container: String,
    pub freshness: Freshness,
}

/// What an Update did to a container, for the UI to word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerUpdate {
    pub name: String,
    pub outcome: UpdateOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// Its image reference still names the image it runs.
    UpToDate,
    /// Replaced by a container made from the newer image.
    Updated,
    /// Replaced, but the old container could not be removed and is kept under this name.
    UpdatedKeptOld(String),
    /// Failed with `error`, and the original could not be put back as it was: it is
    /// kept as `kept_as`, and `stopped` though it ran before.
    NotRestored {
        error: String,
        kept_as: String,
        stopped: bool,
    },
}

impl From<engine::UpdateOutcome> for UpdateOutcome {
    fn from(o: engine::UpdateOutcome) -> Self {
        match o {
            engine::UpdateOutcome::UpToDate => Self::UpToDate,
            engine::UpdateOutcome::Updated { leftover: None, .. } => Self::Updated,
            engine::UpdateOutcome::Updated {
                leftover: Some(old),
                ..
            } => Self::UpdatedKeptOld(old),
        }
    }
}

/// A container that failed on its own: it exited with a non-zero code without being
/// stopped or killed through the daemon, the kernel killed it for running out of
/// memory, or its health check started failing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerExit {
    pub id: String,
    pub name: String,
    pub cause: ExitCause,
}

/// Why a [`ContainerExit`] is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCause {
    /// Stopped on its own with this non-zero exit code.
    Code(i64),
    /// Killed by the kernel for running out of memory (with the exit code it got).
    OutOfMemory(i64),
    /// Still running, but its health check started failing.
    Unhealthy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeCommand {
    Up,
    Down,
    Restart,
    Stop,
    Start,
    Pull,
}

impl From<ComposeCommand> for engine::ComposeCommand {
    fn from(c: ComposeCommand) -> Self {
        use engine::ComposeCommand as C;
        match c {
            ComposeCommand::Up => C::Up,
            ComposeCommand::Down => C::Down,
            ComposeCommand::Restart => C::Restart,
            ComposeCommand::Stop => C::Stop,
            ComposeCommand::Start => C::Start,
            ComposeCommand::Pull => C::Pull,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextEntry {
    pub name: String,
    pub host: String,
    /// The docker CLI's current context.
    pub current: bool,
    pub supported: bool,
}

// ---- streams ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogStream {
    Stdout,
    Stderr,
    Console,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogChunk {
    pub stream: LogStream,
    pub text: String,
}

/// Log output batched over ~50ms. `ended` is set once, on the last batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogBatch {
    pub chunks: Vec<LogChunk>,
    pub ended: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StatsSample {
    pub cpu_percent: f64,
    pub mem_usage: f64,
    pub mem_limit: f64,
    pub net_rx: f64,
    pub net_tx: f64,
    pub pids: f64,
}

impl From<engine::StatsSample> for StatsSample {
    fn from(s: engine::StatsSample) -> Self {
        StatsSample {
            cpu_percent: s.cpu_percent,
            mem_usage: s.mem_usage as f64,
            mem_limit: s.mem_limit as f64,
            net_rx: s.net_rx as f64,
            net_tx: s.net_tx as f64,
            pids: s.pids as f64,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StatsEvent {
    Sample(StatsSample),
    Ended { error: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecOutput {
    /// Base64 of raw terminal bytes (they may split UTF-8 sequences).
    Data {
        base64: String,
    },
    Exit,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_update_outcome_says_whether_the_old_container_was_left() {
        assert_eq!(
            UpdateOutcome::from(engine::UpdateOutcome::UpToDate),
            UpdateOutcome::UpToDate
        );
        let updated = |leftover: Option<&str>| engine::UpdateOutcome::Updated {
            id: "new".into(),
            leftover: leftover.map(str::to_owned),
        };
        assert_eq!(UpdateOutcome::from(updated(None)), UpdateOutcome::Updated);
        assert_eq!(
            UpdateOutcome::from(updated(Some("web-replaced-0123"))),
            UpdateOutcome::UpdatedKeptOld("web-replaced-0123".into())
        );
    }

    #[test]
    fn engine_errors_map_to_kinds() {
        let e = engine::EngineError::PermissionDenied("/var/run/docker.sock".into());
        assert_eq!(UiError::from(&e).kind, UiErrorKind::PermissionDenied);
        let e = engine::EngineError::Daemon {
            status: 500,
            message: "x".into(),
        };
        assert_eq!(UiError::from(&e).kind, UiErrorKind::Daemon);
    }

    #[test]
    fn container_conversion_keeps_only_published_ports() {
        let c = engine::ContainerSummary {
            id: "abc".into(),
            name: "web".into(),
            image: "nginx".into(),
            state: engine::ContainerState::Running,
            status: "Up".into(),
            created: 1_700_000_000,
            ports: vec![
                engine::PortMapping {
                    ip: None,
                    private_port: 80,
                    public_port: Some(8080),
                    protocol: "tcp".into(),
                },
                engine::PortMapping {
                    ip: None,
                    private_port: 443,
                    public_port: None,
                    protocol: "tcp".into(),
                },
            ],
            compose: None,
            networks: vec!["bridge".into()],
            volumes: Vec::new(),
        };
        let d = Container::from(&c);
        assert_eq!(d.ports, vec!["0.0.0.0:8080→80/tcp"]);
        assert_eq!(d.state, ContainerState::Running);
        assert_eq!(d.created, 1_700_000_000.0);
    }

    #[test]
    fn compose_labels_dto_omits_host_filesystem_paths() {
        // A-007 / R-13
        let c = engine::ContainerSummary {
            id: "abc".into(),
            name: "web".into(),
            image: "nginx".into(),
            state: engine::ContainerState::Running,
            status: "Up".into(),
            created: 1_700_000_000,
            ports: Vec::new(),
            compose: Some(engine::ComposeLabels {
                project: "stack".into(),
                service: "api".into(),
                working_dir: Some("/secret/host/dir".into()),
                config_files: vec!["/secret/host/dir/compose.yaml".into()],
            }),
            networks: Vec::new(),
            volumes: Vec::new(),
        };
        assert_eq!(
            Container::from(&c).compose,
            Some(ComposeLabels {
                project: "stack".into(),
                service: "api".into(),
            })
        );
    }

    #[test]
    fn cleanup_scope_and_report_convert_cleanly() {
        let mut scope = CleanupScope::default();
        assert!(scope.is_empty());
        scope.containers = true;
        scope.images = Some(CleanupImages::AllUnused);
        scope.volumes = Some(CleanupVolumes::Anonymous);
        assert!(!scope.is_empty());

        let eng: engine::CleanupScope = scope.into();
        assert!(eng.containers);
        assert_eq!(eng.images, Some(engine::CleanupImages::AllUnused));
        assert_eq!(eng.volumes, Some(engine::CleanupVolumes::Anonymous));

        let report = CleanupReport::from(engine::CleanupReport {
            containers_deleted: 2,
            networks_deleted: 1,
            images_deleted: 0,
            volumes_deleted: 0,
            build_cache_deleted: 0,
            reclaimed: 4096,
            partial_error: Some((engine::CleanupStep::Images, "boom".into())),
        });
        assert_eq!(report.total_deleted(), 3);
        assert_eq!(report.reclaimed, 4096.0);
        assert_eq!(
            report.partial_error,
            Some((CleanupStep::Images, "boom".into()))
        );
    }
}
