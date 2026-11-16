//! Application state. The backend owns it; the UI renders snapshots of it.
//!
//! - Every connect bumps `generation`; results, retries and streams from an older
//!   generation are dropped, so `retry_now` or a context switch always wins.
//! - Lists are fetched in full on connect, then re-listed per kind when the daemon's
//!   event stream says that kind changed (debounced). Missed events converge on reconnect.
//! - Changes are pushed to the UI as delta patches, throttled to one per `EMIT_EVERY`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use engine::{
    ComposeOutput, ConnectTarget, Engine, EngineError, EngineInfo, PullProgress, ResourceKind,
};
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use tokio::task::JoinHandle;

use crate::backend::{Backend, UiEvent};

use crate::dto::{self, ConnState, Notice, Op, RunResult, Snapshot, StatePatch, Target, UiError};

const MAX_BACKOFF_SECS: u32 = 30;
const DEBOUNCE: Duration = Duration::from_millis(100);
const EMIT_EVERY: Duration = Duration::from_millis(50);
const USAGE_POLL_EVERY: Duration = Duration::from_secs(2);
const FIRST_CPU_SAMPLE_DELAY: Duration = Duration::from_millis(500);
const DISK_POLL_EVERY: Duration = Duration::from_secs(30);
/// Compose output kept per project for display.
const COMPOSE_LINES: usize = 500;
/// Maximum finished compose runs retained in memory.
const MAX_COMPOSE_RUNS: usize = 8;

pub fn backoff_secs(attempt: u32) -> u32 {
    (1u32 << attempt.min(5)).min(MAX_BACKOFF_SECS)
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct PullState {
    pub status: String,
    /// layer id → (current, total)
    pub layers: BTreeMap<String, (i64, i64)>,
}

impl PullState {
    pub fn update(&mut self, p: PullProgress) {
        self.status = match &p.layer {
            Some(l) => format!("{l}: {}", p.status),
            None => p.status.clone(),
        };
        let Some(layer) = p.layer else { return };
        if p.status.starts_with("Downloading") {
            if let (Some(cur), Some(total)) = (p.current, p.total) {
                self.layers.insert(layer, (cur, total));
            }
        } else if ["Download complete", "Pull complete", "Already exists"]
            .iter()
            .any(|s| p.status.starts_with(s))
            && let Some(entry) = self.layers.get_mut(&layer)
        {
            entry.0 = entry.1;
        }
    }

    pub fn fraction(&self) -> Option<f32> {
        let (cur, total) = self
            .layers
            .values()
            .fold((0i64, 0i64), |(c, t), (lc, lt)| (c + lc, t + lt));
        (total > 0).then(|| (cur as f32 / total as f32).clamp(0.0, 1.0))
    }
}

pub(crate) struct TaskHandle(pub(crate) JoinHandle<()>);

impl TaskHandle {
    fn abort(&self) {
        self.0.abort();
    }
}

pub(crate) trait ClientOps: Clone + Send + Sync + 'static {
    fn events(&self) -> BoxStream<'static, Result<engine::EngineEvent, EngineError>>;
    fn containers(
        &self,
        all: bool,
    ) -> BoxFuture<'static, Result<Vec<engine::ContainerSummary>, EngineError>>;
    fn images(&self) -> BoxFuture<'static, Result<Vec<engine::ImageSummary>, EngineError>>;
    fn volumes(&self) -> BoxFuture<'static, Result<Vec<engine::VolumeSummary>, EngineError>>;
    fn networks(&self) -> BoxFuture<'static, Result<Vec<engine::NetworkSummary>, EngineError>>;
    fn pull(&self, reference: &str) -> BoxStream<'static, Result<PullProgress, EngineError>>;
    fn system_capacity(&self) -> BoxFuture<'static, Result<engine::SystemCapacity, EngineError>>;
    fn container_usage(
        &self,
        ids: Vec<String>,
    ) -> BoxFuture<'static, Result<engine::ContainerUsageSample, EngineError>>;
    fn disk_usage(&self) -> BoxFuture<'static, Result<Option<engine::DiskUsage>, EngineError>>;
}

impl ClientOps for Engine {
    fn events(&self) -> BoxStream<'static, Result<engine::EngineEvent, EngineError>> {
        Box::pin(Engine::events(self))
    }

    fn containers(
        &self,
        all: bool,
    ) -> BoxFuture<'static, Result<Vec<engine::ContainerSummary>, EngineError>> {
        Box::pin(Engine::containers(self, all))
    }

    fn images(&self) -> BoxFuture<'static, Result<Vec<engine::ImageSummary>, EngineError>> {
        Box::pin(Engine::images(self))
    }

    fn volumes(&self) -> BoxFuture<'static, Result<Vec<engine::VolumeSummary>, EngineError>> {
        Box::pin(Engine::volumes(self))
    }

    fn networks(&self) -> BoxFuture<'static, Result<Vec<engine::NetworkSummary>, EngineError>> {
        Box::pin(Engine::networks(self))
    }

    fn pull(&self, reference: &str) -> BoxStream<'static, Result<PullProgress, EngineError>> {
        Box::pin(Engine::pull(self, reference))
    }

    fn system_capacity(&self) -> BoxFuture<'static, Result<engine::SystemCapacity, EngineError>> {
        Box::pin(Engine::system_capacity(self))
    }

    fn container_usage(
        &self,
        ids: Vec<String>,
    ) -> BoxFuture<'static, Result<engine::ContainerUsageSample, EngineError>> {
        Box::pin(Engine::container_usage(self, ids))
    }

    fn disk_usage(&self) -> BoxFuture<'static, Result<Option<engine::DiskUsage>, EngineError>> {
        Box::pin(Engine::disk_usage(self))
    }
}

pub(crate) trait Driver: Clone + Send + Sync + 'static {
    type Client: ClientOps;

    fn state(&self) -> &AppState<Self::Client>;
    fn connect(
        &self,
        target: ConnectTarget,
    ) -> BoxFuture<'static, Result<(Self::Client, EngineInfo), EngineError>>;
    fn run_compose(
        &self,
        target: &ConnectTarget,
        labels: &engine::ComposeLabels,
        command: engine::ComposeCommand,
    ) -> BoxStream<'static, ComposeOutput>;
    /// Starts the local daemon behind `target` (`engine::start_daemon`).
    fn start_daemon(
        &self,
        target: ConnectTarget,
    ) -> BoxFuture<'static, Result<&'static str, EngineError>>;
    fn clear_streams(&self);
    fn emit_patch(&self, patch: StatePatch);
    fn emit_notice(&self, notice: Notice);
    /// Reports a container that failed on its own.
    fn emit_exit(&self, exit: dto::ContainerExit);
    fn spawn<F>(&self, fut: F) -> TaskHandle
    where
        F: Future<Output = ()> + Send + 'static;

    fn debounce_duration(&self) -> Duration {
        DEBOUNCE
    }

    fn emit_duration(&self) -> Duration {
        EMIT_EVERY
    }

    fn usage_poll_duration(&self) -> Duration {
        USAGE_POLL_EVERY
    }

    fn first_cpu_sample_duration(&self) -> Duration {
        FIRST_CPU_SAMPLE_DELAY
    }

    fn disk_poll_duration(&self) -> Duration {
        DISK_POLL_EVERY
    }
}

#[derive(Clone)]
struct NativeDriver(Backend);

impl Driver for NativeDriver {
    type Client = Engine;

    fn state(&self) -> &AppState<Engine> {
        self.0.state()
    }

    fn connect(
        &self,
        target: ConnectTarget,
    ) -> BoxFuture<'static, Result<(Engine, EngineInfo), EngineError>> {
        Box::pin(Engine::connect(target))
    }

    fn run_compose(
        &self,
        target: &ConnectTarget,
        labels: &engine::ComposeLabels,
        command: engine::ComposeCommand,
    ) -> BoxStream<'static, ComposeOutput> {
        Box::pin(engine::run_compose(target, labels, command))
    }

    fn start_daemon(
        &self,
        target: ConnectTarget,
    ) -> BoxFuture<'static, Result<&'static str, EngineError>> {
        Box::pin(engine::start_daemon(target))
    }

    fn clear_streams(&self) {
        self.0.streams().clear();
    }

    fn emit_patch(&self, patch: StatePatch) {
        // Fails only when the window is gone, and then nobody is listening.
        self.0.send(UiEvent::State(Box::new(patch)));
    }

    fn emit_notice(&self, notice: Notice) {
        self.0.send(UiEvent::Notice(notice));
    }

    fn emit_exit(&self, exit: dto::ContainerExit) {
        self.0.send(UiEvent::ContainerExited(exit));
    }

    fn spawn<F>(&self, fut: F) -> TaskHandle
    where
        F: Future<Output = ()> + Send + 'static,
    {
        TaskHandle(self.0.spawn(fut))
    }
}

#[derive(Default, Clone, Copy)]
struct DirtySlices {
    containers: bool,
    images: bool,
    volumes: bool,
    networks: bool,
    pending: bool,
    pulls: bool,
    compose_runs: bool,
    usage: bool,
}

struct InternalComposeRun {
    project: String,
    command: String,
    lines: VecDeque<String>,
    result: RunResult,
    seq: u64,
}

impl InternalComposeRun {
    fn to_dto(&self) -> dto::ComposeRun {
        dto::ComposeRun {
            project: self.project.clone(),
            command: self.command.clone(),
            lines: self.lines.iter().cloned().collect(),
            result: self.result.clone(),
        }
    }
}

struct Inner<C = Engine> {
    target: ConnectTarget,
    conn: ConnState,
    engine: Option<C>,
    generation: u64,
    attempt: u32,
    revision: u32,
    containers: Vec<engine::ContainerSummary>,
    images: Vec<engine::ImageSummary>,
    volumes: Vec<engine::VolumeSummary>,
    networks: Vec<engine::NetworkSummary>,
    pending: BTreeSet<String>,
    /// The busy keys `run_op` took, each with the operation holding it. They stay
    /// taken across a reconnect until that operation ends: its request keeps running.
    op_keys: HashMap<String, u64>,
    next_op: u64,
    pulls: BTreeMap<String, PullState>,
    pull_tasks: BTreeMap<String, TaskHandle>,
    compose_runs: BTreeMap<String, InternalComposeRun>,
    next_compose_seq: u64,
    compose_tasks: BTreeMap<String, TaskHandle>,
    compose_available: Option<bool>,
    usage: dto::SystemUsage,
    /// Per-volume sizes from the last `/system/df`.
    volume_sizes: BTreeMap<String, u64>,
    daemon_startable: Option<&'static str>,
    daemon_starting: Option<&'static str>,
    containers_loaded_gen: u64,
    usage_notify: Arc<tokio::sync::Notify>,
    disk_notify: Arc<tokio::sync::Notify>,
    dirty: HashSet<ResourceKind>,
    dirty_slices: DirtySlices,
    next_list_seq: u64,
    applied_list_seq: HashMap<ResourceKind, u64>,
    /// The generation each kind was last listed in. The first list of a connection is
    /// always sent; later ones only when they differ.
    listed_gen: HashMap<ResourceKind, u64>,
    refresh_scheduled: bool,
    emit_scheduled: bool,
    connect_task: Option<TaskHandle>,
    retry_task: Option<TaskHandle>,
    events_task: Option<TaskHandle>,
    usage_task: Option<TaskHandle>,
    disk_task: Option<TaskHandle>,
    /// Tells crashes from stops in the event stream.
    exits: crate::exits::ExitWatch,
}

pub struct AppState<C = Engine>(Mutex<Inner<C>>);

/// What to do after a connect attempt (or a lost connection).
#[derive(Debug, PartialEq, Eq)]
enum Next {
    Done,
    RetryIn(u32),
}

impl<C> Inner<C> {
    fn new(target: ConnectTarget) -> Self {
        Self {
            daemon_startable: engine::daemon_starter(&target),
            daemon_starting: None,
            volume_sizes: BTreeMap::new(),
            target,
            conn: ConnState::Connecting,
            engine: None,
            generation: 0,
            attempt: 0,
            revision: 0,
            containers: Vec::new(),
            images: Vec::new(),
            volumes: Vec::new(),
            networks: Vec::new(),
            pending: BTreeSet::new(),
            op_keys: HashMap::new(),
            next_op: 0,
            pulls: BTreeMap::new(),
            pull_tasks: BTreeMap::new(),
            compose_runs: BTreeMap::new(),
            next_compose_seq: 0,
            compose_tasks: BTreeMap::new(),
            compose_available: None,
            usage: dto::SystemUsage::default(),
            containers_loaded_gen: 0,
            usage_notify: Arc::new(tokio::sync::Notify::new()),
            disk_notify: Arc::new(tokio::sync::Notify::new()),
            dirty: HashSet::new(),
            dirty_slices: DirtySlices::default(),
            next_list_seq: 0,
            applied_list_seq: HashMap::new(),
            listed_gen: HashMap::new(),
            refresh_scheduled: false,
            emit_scheduled: false,
            connect_task: None,
            retry_task: None,
            events_task: None,
            usage_task: None,
            disk_task: None,
            exits: crate::exits::ExitWatch::default(),
        }
    }

    fn update_usage(&mut self, f: impl FnOnce(&mut dto::SystemUsage)) -> bool {
        let mut next = self.usage;
        f(&mut next);
        if next != self.usage {
            self.usage = next;
            self.dirty_slices.usage = true;
            true
        } else {
            false
        }
    }

    fn allocate_list_seq(&mut self) -> u64 {
        self.next_list_seq += 1;
        self.next_list_seq
    }

    fn accept_list_seq(&mut self, kind: ResourceKind, seq: u64) -> bool {
        let applied = self.applied_list_seq.get(&kind).copied().unwrap_or(0);
        if seq <= applied {
            return false;
        }
        self.applied_list_seq.insert(kind, seq);
        true
    }

    fn evict_finished_compose_runs(&mut self) {
        while self.compose_runs.len() > MAX_COMPOSE_RUNS {
            let oldest = self
                .compose_runs
                .iter()
                .filter(|(_, r)| !matches!(r.result, RunResult::Running))
                .min_by_key(|(_, r)| r.seq)
                .map(|(k, _)| k.clone());
            let Some(project) = oldest else {
                break;
            };
            self.compose_runs.remove(&project);
            self.dirty_slices.compose_runs = true;
        }
    }

    fn abort_in_flight(&mut self) {
        let before = self.pending.len();
        let ops = &self.op_keys;
        self.pending.retain(|k| ops.contains_key(k));
        if self.pending.len() != before {
            self.dirty_slices.pending = true;
        }
        if !self.pulls.is_empty() {
            self.pulls.clear();
            self.dirty_slices.pulls = true;
        }
        for (_, task) in std::mem::take(&mut self.pull_tasks) {
            task.abort();
        }
        for (_, task) in std::mem::take(&mut self.compose_tasks) {
            task.abort();
        }
        for run in self.compose_runs.values_mut() {
            if matches!(run.result, RunResult::Running) {
                run.result = RunResult::Failed {
                    message: "Interrupted by reconnect.".into(),
                };
                self.dirty_slices.compose_runs = true;
            }
        }
        self.evict_finished_compose_runs();
        if let Some(t) = self.events_task.take() {
            t.abort();
        }
        if let Some(t) = self.usage_task.take() {
            t.abort();
        }
        if let Some(t) = self.disk_task.take() {
            t.abort();
        }
    }

    fn dto_pulls(&self) -> Vec<dto::Pull> {
        self.pulls
            .iter()
            .map(|(r, p)| dto::Pull {
                reference: r.clone(),
                status: p.status.clone(),
                fraction: p.fraction(),
            })
            .collect()
    }

    fn dto_compose_runs(&self) -> Vec<dto::ComposeRun> {
        self.compose_runs
            .values()
            .map(InternalComposeRun::to_dto)
            .collect()
    }

    fn dto_volumes(&self) -> Vec<dto::Volume> {
        self.volumes
            .iter()
            .map(|v| dto::Volume::new(v, self.volume_sizes.get(&v.name).copied()))
            .collect()
    }

    fn dto_daemon(&self) -> dto::DaemonStatus {
        dto::DaemonStatus {
            startable: self.daemon_startable.map(str::to_owned),
            starting: self.daemon_starting.map(str::to_owned),
            ready_timeout_secs: engine::DAEMON_READY_TIMEOUT_SECS as u32,
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            revision: self.revision,
            conn: self.conn.clone(),
            target: Target::from(&self.target),
            containers: self.containers.iter().map(Into::into).collect(),
            images: self.images.iter().map(Into::into).collect(),
            volumes: self.dto_volumes(),
            networks: self.networks.iter().map(Into::into).collect(),
            pending: self.pending.iter().cloned().collect(),
            pulls: self.dto_pulls(),
            compose_runs: self.dto_compose_runs(),
            compose_available: self.compose_available,
            usage: self.usage,
            daemon: self.dto_daemon(),
        }
    }

    fn take_patch(&mut self) -> StatePatch {
        let d = std::mem::take(&mut self.dirty_slices);
        tracing::debug!(
            revision = self.revision,
            containers = d.containers,
            images = d.images,
            volumes = d.volumes,
            networks = d.networks,
            pending = d.pending,
            pulls = d.pulls,
            compose_runs = d.compose_runs,
            usage = d.usage,
            "building state patch"
        );
        StatePatch {
            revision: self.revision,
            conn: self.conn.clone(),
            target: Target::from(&self.target),
            containers: d
                .containers
                .then(|| self.containers.iter().map(Into::into).collect()),
            images: d
                .images
                .then(|| self.images.iter().map(Into::into).collect()),
            volumes: d.volumes.then(|| self.dto_volumes()),
            networks: d
                .networks
                .then(|| self.networks.iter().map(Into::into).collect()),
            pending: d.pending.then(|| self.pending.iter().cloned().collect()),
            pulls: d.pulls.then(|| self.dto_pulls()),
            compose_runs: d.compose_runs.then(|| self.dto_compose_runs()),
            compose_available: self.compose_available,
            usage: d.usage.then_some(self.usage),
            daemon: self.dto_daemon(),
        }
    }

    fn apply(&mut self, result: Result<(C, EngineInfo), EngineError>) -> Next {
        match result {
            Ok((engine, info)) => {
                self.attempt = 0;
                self.engine = Some(engine);
                self.conn = ConnState::Connected {
                    info: (&info).into(),
                };
                Next::Done
            }
            Err(e) => self.apply_error(&e),
        }
    }

    /// The connection failed or broke. Keeps lists (shown stale) when it had been up.
    fn apply_error(&mut self, e: &EngineError) -> Next {
        self.engine = None;
        self.abort_in_flight();
        if matches!(e, EngineError::PermissionDenied(_)) {
            tracing::warn!(target = %self.target.host, error = %e, "engine permission denied");
            self.conn = ConnState::PermissionDenied {
                error: UiError::from(e),
            };
            return Next::Done;
        }
        let retry_in = backoff_secs(self.attempt);
        self.attempt = self.attempt.saturating_add(1);
        tracing::warn!(target = %self.target.host, retry_in, error = %e, "engine unreachable; scheduling retry");
        let was_up = matches!(
            self.conn,
            ConnState::Connected { .. } | ConnState::Reconnecting { .. }
        );
        self.conn = if was_up {
            ConnState::Reconnecting {
                error: UiError::from(e),
                retry_in,
            }
        } else {
            ConnState::Unreachable {
                error: UiError::from(e),
                retry_in,
            }
        };
        Next::RetryIn(retry_in)
    }

    fn clear_lists(&mut self) {
        self.volume_sizes.clear();
        self.containers.clear();
        self.images.clear();
        self.volumes.clear();
        self.networks.clear();
        self.pulls.clear();
        self.compose_runs.clear();
        self.usage = dto::SystemUsage::default();
        self.dirty_slices = DirtySlices {
            containers: true,
            images: true,
            volumes: true,
            networks: true,
            pending: true,
            pulls: true,
            compose_runs: true,
            usage: true,
        };
    }
}

impl AppState<Engine> {
    pub fn new(target: ConnectTarget) -> Self {
        AppState(Mutex::new(Inner::new(target)))
    }
}

impl<C> AppState<C> {
    #[cfg(test)]
    pub(crate) fn with_client(target: ConnectTarget) -> Self {
        AppState(Mutex::new(Inner::new(target)))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<C>> {
        self.0.lock().expect("state lock poisoned")
    }

    pub fn snapshot(&self) -> Snapshot {
        self.lock().snapshot()
    }

    /// The names of every container listed (running or not).
    pub fn container_names(&self) -> Vec<String> {
        self.lock()
            .containers
            .iter()
            .map(|c| c.name.clone())
            .collect()
    }

    pub fn reset_backoff(&self) {
        self.lock().attempt = 0;
    }

    pub fn engine(&self) -> Result<C, UiError>
    where
        C: Clone,
    {
        self.lock().engine.clone().ok_or_else(|| UiError {
            kind: dto::UiErrorKind::Transport,
            message: "Not connected to Docker Engine.".into(),
        })
    }

    /// Compose labels of a project, taken from its containers (never from the UI).
    pub fn compose_labels(&self, project: &str) -> Option<engine::ComposeLabels> {
        let inner = self.lock();
        inner
            .containers
            .iter()
            .find_map(|c| c.compose.clone().filter(|l| l.project == project))
    }
}

/// Marks a change; a delta patch is emitted at most once per `EMIT_EVERY`.
fn changed_with<D: Driver>(driver: &D) {
    let schedule = {
        let mut inner = driver.state().lock();
        inner.revision = inner.revision.wrapping_add(1);
        !std::mem::replace(&mut inner.emit_scheduled, true)
    };
    if schedule {
        let task_driver = driver.clone();
        let delay = driver.emit_duration();
        let _ = driver.spawn(async move {
            tokio::time::sleep(delay).await;
            let patch = {
                let mut inner = task_driver.state().lock();
                inner.emit_scheduled = false;
                inner.take_patch()
            };
            task_driver.emit_patch(patch);
        });
    }
}

pub fn notice(app: &Backend, notice: Notice) {
    NativeDriver(app.clone()).emit_notice(notice);
}

// ---- connection ----

/// Connects to `target` (or the current one), superseding any earlier attempt.
pub fn connect(app: &Backend, new_target: Option<ConnectTarget>) {
    connect_with(&NativeDriver(app.clone()), new_target);
}

/// At launch: if the local daemon for the current target is down and the app
/// knows how to start it, starts it.
pub fn start_daemon_if_down(app: &Backend) {
    let driver = NativeDriver(app.clone());
    let target = {
        let inner = driver.state().lock();
        if inner.daemon_startable.is_none() {
            return;
        }
        inner.target.clone()
    };
    let _ = driver.clone().spawn(async move {
        if engine::daemon_is_down(target).await {
            // Losing the race to a manual start is fine; it reports itself.
            let _ = start_daemon_with(&driver);
        }
    });
}

/// Starts the local daemon for the current target, then reconnects at once
/// instead of waiting out the backoff.
pub fn start_daemon(app: &Backend) -> Result<(), UiError> {
    start_daemon_with(&NativeDriver(app.clone()))
}

fn start_daemon_with<D: Driver>(driver: &D) -> Result<(), UiError> {
    let (target, label) = {
        let mut inner = driver.state().lock();
        let Some(label) = inner.daemon_startable else {
            return Err(UiError {
                kind: dto::UiErrorKind::Local,
                message: format!(
                    "Docker Stack cannot start the daemon behind {}.",
                    inner.target.host
                ),
            });
        };
        if inner.daemon_starting.is_some() {
            return Err(busy(&format!("Starting {label}")));
        }
        inner.daemon_starting = Some(label);
        (inner.target.clone(), label)
    };
    changed_with(driver);
    let task_driver = driver.clone();
    let _ = driver.spawn(async move {
        let result = task_driver.start_daemon(target.clone()).await;
        let reconnect = {
            let mut inner = task_driver.state().lock();
            inner.daemon_starting = None;
            let reconnect = result.is_ok() && inner.target == target && inner.engine.is_none();
            if reconnect {
                inner.attempt = 0;
            }
            reconnect
        };
        changed_with(&task_driver);
        match result {
            Ok(_) => task_driver.emit_notice(Notice::DaemonStarted(label.to_owned())),
            Err(e) => {
                tracing::warn!(error = %e, "could not start the docker daemon");
                task_driver.emit_notice(Notice::Error(e.to_string()));
            }
        }
        if reconnect {
            connect_with(&task_driver, None);
        }
    });
    Ok(())
}

fn connect_with<D: Driver>(driver: &D, new_target: Option<ConnectTarget>) {
    driver.clear_streams();
    let (generation, target) = {
        let mut inner = driver.state().lock();
        if let Some(t) = inner.connect_task.take() {
            t.abort();
        }
        if let Some(t) = inner.retry_task.take() {
            t.abort();
        }
        if let Some(t) = new_target.filter(|t| *t != inner.target) {
            tracing::info!(host = %t.host, source = %t.source, "switching docker target");
            inner.daemon_startable = engine::daemon_starter(&t);
            inner.target = t;
            inner.attempt = 0;
            inner.clear_lists();
            inner.conn = ConnState::Connecting;
        }
        inner.generation += 1;
        inner.engine = None;
        inner.abort_in_flight();
        if !matches!(inner.conn, ConnState::Reconnecting { .. }) {
            inner.conn = ConnState::Connecting;
        }
        (inner.generation, inner.target.clone())
    };
    changed_with(driver);
    let task_driver = driver.clone();
    let connect_task = driver.spawn(async move {
        let result = task_driver.connect(target).await;
        let (next, engine) = {
            let mut inner = task_driver.state().lock();
            if inner.generation != generation {
                return;
            }
            inner.connect_task = None;
            let next = inner.apply(result);
            (next, inner.engine.clone())
        };
        changed_with(&task_driver);
        match (next, engine) {
            (Next::Done, Some(engine)) => {
                start_events_with(&task_driver, generation, engine.clone());
                start_usage_with(&task_driver, generation, engine);
                for k in [
                    ResourceKind::Container,
                    ResourceKind::Image,
                    ResourceKind::Volume,
                    ResourceKind::Network,
                ] {
                    refresh_with(&task_driver, k);
                }
            }
            (Next::RetryIn(secs), _) => schedule_retry_with(&task_driver, generation, secs),
            _ => {}
        }
    });
    let mut inner = driver.state().lock();
    if inner.generation == generation
        && inner.engine.is_none()
        && matches!(
            inner.conn,
            ConnState::Connecting | ConnState::Reconnecting { .. }
        )
    {
        // Store only if the spawned task hasn't already completed synchronously in tests.
        if inner.connect_task.is_none() {
            inner.connect_task = Some(connect_task);
        }
    } else if inner.generation != generation {
        connect_task.abort();
    }
}

fn schedule_retry_with<D: Driver>(driver: &D, generation: u64, secs: u32) {
    let task_driver = driver.clone();
    let task = driver.spawn(async move {
        tokio::time::sleep(Duration::from_secs(secs.into())).await;
        let should_connect = {
            let mut inner = task_driver.state().lock();
            if inner.generation == generation {
                inner.retry_task = None;
                true
            } else {
                false
            }
        };
        if should_connect {
            connect_with(&task_driver, None);
        }
    });
    let mut inner = driver.state().lock();
    if inner.generation == generation {
        inner.retry_task = Some(task);
    } else {
        task.abort();
    }
}

fn connection_lost_with<D: Driver>(driver: &D, generation: u64, e: &EngineError) {
    let next = {
        let mut inner = driver.state().lock();
        if inner.generation != generation || inner.engine.is_none() {
            return;
        }
        inner.apply_error(e)
    };
    driver.clear_streams();
    changed_with(driver);
    if let Next::RetryIn(secs) = next {
        schedule_retry_with(driver, generation, secs);
    }
}

fn start_events_with<D: Driver>(driver: &D, generation: u64, engine: D::Client) {
    let mut inner = driver.state().lock();
    if inner.generation != generation {
        return;
    }
    let task_driver = driver.clone();
    let task = driver.spawn(async move {
        let mut rx = engine.events();
        while let Some(item) = rx.next().await {
            match item {
                Ok(ev) => on_event_with(&task_driver, generation, ev),
                Err(e) => {
                    connection_lost_with(&task_driver, generation, &e);
                    return;
                }
            }
        }
    });
    inner.events_task = Some(task);
}

fn apply_system_capacity<D: Driver>(driver: &D, generation: u64, cap: engine::SystemCapacity) {
    let changed = {
        let mut inner = driver.state().lock();
        if inner.generation != generation {
            return;
        }
        inner.update_usage(|u| {
            u.cpu_cores = (cap.cpu_cores > 0).then_some(cap.cpu_cores);
            u.mem_total = (cap.mem_total > 0).then_some(cap.mem_total as f64);
            u.disk_total = cap.disk_total.map(|n| n as f64);
        })
    };
    if changed {
        changed_with(driver);
    }
}

fn refresh_capacity_with<D: Driver>(driver: &D) {
    let (generation, engine) = {
        let inner = driver.state().lock();
        let Some(e) = inner.engine.clone() else {
            return;
        };
        (inner.generation, e)
    };
    let task_driver = driver.clone();
    let _ = driver.spawn(async move {
        match engine.system_capacity().await {
            Ok(cap) => apply_system_capacity(&task_driver, generation, cap),
            Err(e) if e.is_connection_lost() => connection_lost_with(&task_driver, generation, &e),
            Err(e) => tracing::debug!(error = %e, "system capacity refresh failed"),
        }
    });
}

fn start_usage_with<D: Driver>(driver: &D, generation: u64, engine: D::Client) {
    let (usage_notify, disk_notify) = {
        let inner = driver.state().lock();
        if inner.generation != generation {
            return;
        }
        (inner.usage_notify.clone(), inner.disk_notify.clone())
    };

    let usage_driver = driver.clone();
    let usage_engine = engine.clone();
    let usage_task = driver.spawn(async move {
        match usage_engine.system_capacity().await {
            Ok(cap) => apply_system_capacity(&usage_driver, generation, cap),
            Err(e) if e.is_connection_lost() => {
                connection_lost_with(&usage_driver, generation, &e);
                return;
            }
            Err(e) => tracing::debug!(error = %e, "initial system capacity fetch failed"),
        }

        let mut prev_cpu: HashMap<String, engine::ContainerCpuReading> = HashMap::new();
        loop {
            let notified = usage_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let (loaded, running_ids, fallback_cpus) = {
                let inner = usage_driver.state().lock();
                if inner.generation != generation {
                    return;
                }
                let loaded = inner.containers_loaded_gen == generation;
                let ids: Vec<String> = inner
                    .containers
                    .iter()
                    .filter(|c| c.state.is_running())
                    .map(|c| c.id.clone())
                    .collect();
                let cpus = inner.usage.cpu_cores.unwrap_or(1);
                (loaded, ids, cpus)
            };

            if !loaded {
                notified.await;
                continue;
            }

            if running_ids.is_empty() {
                prev_cpu.clear();
                let changed = {
                    let mut inner = usage_driver.state().lock();
                    if inner.generation != generation {
                        return;
                    }
                    inner.update_usage(|u| {
                        u.cpu_used_cores = Some(0.0);
                        u.mem_usage = Some(0.0);
                    })
                };
                if changed {
                    changed_with(&usage_driver);
                }
                notified.await;
                continue;
            }

            match usage_engine.container_usage(running_ids).await {
                Ok(sample) => {
                    let cores =
                        engine::aggregate_cpu_cores(&prev_cpu, &sample.cpu_readings, fallback_cpus);
                    let need_fast_followup = cores.is_none() && !sample.cpu_readings.is_empty();
                    prev_cpu = sample.cpu_readings;
                    let changed = {
                        let mut inner = usage_driver.state().lock();
                        if inner.generation != generation {
                            return;
                        }
                        inner.update_usage(|u| {
                            if let Some(c) = cores.filter(|v| v.is_finite()) {
                                u.cpu_used_cores = Some(c);
                            }
                            u.mem_usage = Some(sample.mem_usage as f64);
                        })
                    };
                    if changed {
                        changed_with(&usage_driver);
                    }
                    let wait = if need_fast_followup {
                        usage_driver.first_cpu_sample_duration()
                    } else {
                        usage_driver.usage_poll_duration()
                    };
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = &mut notified => {}
                    }
                }
                Err(e) if e.is_connection_lost() => {
                    connection_lost_with(&usage_driver, generation, &e);
                    return;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "container usage poll failed");
                    tokio::select! {
                        _ = tokio::time::sleep(usage_driver.usage_poll_duration()) => {}
                        _ = &mut notified => {}
                    }
                }
            }
        }
    });

    let disk_driver = driver.clone();
    let disk_task = driver.spawn(async move {
        loop {
            match engine.disk_usage().await {
                Ok(disk) => {
                    let changed = {
                        let mut inner = disk_driver.state().lock();
                        if inner.generation != generation {
                            return;
                        }
                        let sizes = disk
                            .as_ref()
                            .map(|d| d.volume_sizes.clone())
                            .unwrap_or_default();
                        let sizes_changed = sizes != inner.volume_sizes;
                        if sizes_changed {
                            inner.volume_sizes = sizes;
                            inner.dirty_slices.volumes = true;
                        }
                        let usage_changed = inner.update_usage(|u| match &disk {
                            Some(d) => {
                                u.disk_usage = Some(d.total_used() as f64);
                                u.disk_breakdown = Some(d.into());
                            }
                            None => {
                                u.disk_usage = None;
                                u.disk_breakdown = None;
                            }
                        });
                        sizes_changed || usage_changed
                    };
                    if changed {
                        changed_with(&disk_driver);
                    }
                }
                Err(e) if e.is_connection_lost() => {
                    connection_lost_with(&disk_driver, generation, &e);
                    return;
                }
                Err(e) => tracing::debug!(error = %e, "disk usage poll failed"),
            }
            tokio::select! {
                _ = tokio::time::sleep(disk_driver.disk_poll_duration()) => {}
                _ = disk_notify.notified() => {}
            }
        }
    });

    let mut inner = driver.state().lock();
    if inner.generation == generation && inner.engine.is_some() {
        inner.usage_task = Some(usage_task);
        inner.disk_task = Some(disk_task);
    } else {
        usage_task.abort();
        disk_task.abort();
    }
}

fn is_noisy_container_action(base_action: &str) -> bool {
    matches!(
        base_action,
        "exec_create"
            | "exec_start"
            | "exec_die"
            | "exec_detach"
            | "resize"
            | "top"
            | "attach"
            | "detach"
            | "archive-path"
            | "extract-to-dir"
            | "copy"
            | "export"
    )
}

fn on_event_with<D: Driver>(driver: &D, generation: u64, ev: engine::EngineEvent) {
    if ev.kind == ResourceKind::Other {
        return;
    }
    let base_action = ev.action.split(':').next().unwrap_or(&ev.action).trim();
    if ev.kind == ResourceKind::Container && is_noisy_container_action(base_action) {
        return;
    }
    let (schedule, exit) = {
        let mut inner = driver.state().lock();
        if inner.generation != generation {
            return;
        }
        let exit = inner.exits.observe(&ev, Instant::now());
        inner.dirty.insert(ev.kind);
        // Removing or creating a container changes its image's "in use" count.
        if ev.kind == ResourceKind::Container && matches!(base_action, "create" | "destroy") {
            inner.dirty.insert(ResourceKind::Image);
        }
        (!std::mem::replace(&mut inner.refresh_scheduled, true), exit)
    };
    if let Some(exit) = exit {
        driver.emit_exit(exit);
    }
    if schedule {
        let task_driver = driver.clone();
        let delay = driver.debounce_duration();
        let _ = driver.spawn(async move {
            tokio::time::sleep(delay).await;
            let (kinds, disk_notify) = {
                let mut inner = task_driver.state().lock();
                inner.refresh_scheduled = false;
                let kinds: Vec<ResourceKind> = inner.dirty.drain().collect();
                let disk_dirty = kinds
                    .iter()
                    .any(|k| matches!(k, ResourceKind::Image | ResourceKind::Volume));
                (kinds, disk_dirty.then(|| inner.disk_notify.clone()))
            };
            if let Some(n) = disk_notify {
                n.notify_one();
            }
            for k in kinds {
                refresh_with(&task_driver, k);
            }
        });
    }
}

// ---- lists ----

fn refresh_with<D: Driver>(driver: &D, kind: ResourceKind) {
    if kind == ResourceKind::Other {
        return;
    }
    let (generation, seq, engine) = {
        let mut inner = driver.state().lock();
        let Some(e) = inner.engine.clone() else {
            return;
        };
        let seq = inner.allocate_list_seq();
        (inner.generation, seq, e)
    };
    let task_driver = driver.clone();
    let _ = driver.spawn(async move {
        let started_at = Instant::now();
        // Whether the refresh changed anything the UI shows. A re-list that returns the
        // same list as before on this connection (a volume or network re-listed for a
        // container's mount or connect event) emits no patch, so the UI does not rebuild
        // the page for nothing.
        let mut changed = false;
        macro_rules! store {
            ($task:expr, $field:ident, $dirty:ident) => {{
                let result = $task.await;
                match result {
                    Ok(v) => {
                        let count = v.len();
                        let mut inner = task_driver.state().lock();
                        if inner.generation != generation || !inner.accept_list_seq(kind, seq) {
                            return;
                        }
                        let first = inner.listed_gen.insert(kind, generation) != Some(generation);
                        if first || inner.$field != v {
                            inner.$field = v;
                            inner.dirty_slices.$dirty = true;
                            changed = true;
                        }
                        if kind == ResourceKind::Container {
                            inner.containers_loaded_gen = generation;
                            inner.usage_notify.notify_one();
                        }
                        tracing::debug!(
                            ?kind,
                            items = count,
                            changed,
                            elapsed_us = started_at.elapsed().as_micros() as u64,
                            "refreshed resource list"
                        );
                    }
                    Err(e) if e.is_connection_lost() => {
                        return connection_lost_with(&task_driver, generation, &e);
                    }
                    Err(e) => return task_driver.emit_notice(Notice::Error(e.to_string())),
                }
            }};
        }
        match kind {
            ResourceKind::Container => store!(engine.containers(true), containers, containers),
            ResourceKind::Image => store!(engine.images(), images, images),
            ResourceKind::Volume => store!(engine.volumes(), volumes, volumes),
            ResourceKind::Network => store!(engine.networks(), networks, networks),
            ResourceKind::Other => return,
        }
        if changed {
            changed_with(&task_driver);
        }
    });
}

pub fn refresh_all(app: &Backend) {
    refresh_all_with(&NativeDriver(app.clone()));
}

fn refresh_all_with<D: Driver>(driver: &D) {
    refresh_capacity_with(driver);
    driver.state().lock().disk_notify.notify_one();
    for k in [
        ResourceKind::Container,
        ResourceKind::Image,
        ResourceKind::Volume,
        ResourceKind::Network,
    ] {
        refresh_with(driver, k);
    }
}

// ---- operations ----

fn busy(what: &str) -> UiError {
    UiError {
        kind: dto::UiErrorKind::Conflict,
        message: format!("{what} is already in progress."),
    }
}

/// Runs an operation keyed by `key` (refused while one with that key runs, across
/// reconnects too), then re-lists `kinds`. Failures become an error notice naming `what`,
/// also when the operation ends after a reconnect.
///
/// `start` makes the request and is called only once the key is taken: an engine
/// task runs as soon as it is made, so a refused second click sends nothing.
pub fn run_op<T, F>(
    app: &Backend,
    key: String,
    kinds: &'static [ResourceKind],
    what: Op,
    start: impl FnOnce() -> F,
    on_ok: impl FnOnce(&Backend, T) + Send + 'static,
) -> Result<(), UiError>
where
    F: Future<Output = Result<T, EngineError>> + Send + 'static,
    T: Send + 'static,
{
    run_op_with(
        &NativeDriver(app.clone()),
        key,
        kinds,
        what,
        start,
        move |d, v| on_ok(&d.0, v),
    )
}

fn run_op_with<D, T, F>(
    driver: &D,
    key: String,
    kinds: &'static [ResourceKind],
    what: Op,
    start: impl FnOnce() -> F,
    on_ok: impl FnOnce(&D, T) + Send + 'static,
) -> Result<(), UiError>
where
    D: Driver,
    F: Future<Output = Result<T, EngineError>> + Send + 'static,
    T: Send + 'static,
{
    let (generation, op_id) = {
        let mut inner = driver.state().lock();
        if !inner.pending.insert(key.clone()) {
            return Err(busy("An operation on this item"));
        }
        inner.next_op += 1;
        let op_id = inner.next_op;
        inner.op_keys.insert(key.clone(), op_id);
        inner.dirty_slices.pending = true;
        (inner.generation, op_id)
    };
    changed_with(driver);
    let op = start();
    let task_driver = driver.clone();
    let _ = driver.spawn(async move {
        let result = op.await;
        {
            let mut inner = task_driver.state().lock();
            if inner.op_keys.get(&key) == Some(&op_id) {
                inner.op_keys.remove(&key);
                if inner.pending.remove(&key) {
                    inner.dirty_slices.pending = true;
                }
            }
        }
        changed_with(&task_driver);
        match result {
            Ok(v) => {
                on_ok(&task_driver, v);
                if kinds
                    .iter()
                    .any(|k| matches!(k, ResourceKind::Image | ResourceKind::Volume))
                {
                    task_driver.state().lock().disk_notify.notify_one();
                }
                for k in kinds {
                    refresh_with(&task_driver, *k);
                }
            }
            Err(e) if e.is_connection_lost() => connection_lost_with(&task_driver, generation, &e),
            Err(e) => task_driver.emit_notice(Notice::Failed {
                op: what,
                error: e.to_string(),
            }),
        }
    });
    Ok(())
}

pub fn pull(app: &Backend, engine: Engine, reference: String) -> Result<(), UiError> {
    pull_with(&NativeDriver(app.clone()), engine, reference)
}

fn pull_with<D: Driver>(driver: &D, engine: D::Client, reference: String) -> Result<(), UiError> {
    {
        let mut inner = driver.state().lock();
        if inner.pulls.contains_key(&reference) {
            return Err(busy(&format!("Pulling {reference}")));
        }
        let generation = inner.generation;
        inner.pulls.insert(
            reference.clone(),
            PullState {
                status: "Starting…".into(),
                ..Default::default()
            },
        );
        inner.dirty_slices.pulls = true;
        let task_driver = driver.clone();
        let task_ref = reference.clone();
        let task = driver.spawn(async move {
            let mut rx = engine.pull(&task_ref);
            let mut failure = None;
            while let Some(item) = rx.next().await {
                match item {
                    Ok(p) => {
                        let mut inner = task_driver.state().lock();
                        if inner.generation != generation {
                            return;
                        }
                        if let Some(s) = inner.pulls.get_mut(&task_ref) {
                            s.update(p);
                            inner.dirty_slices.pulls = true;
                        }
                        drop(inner);
                        changed_with(&task_driver);
                    }
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
                }
            }
            {
                let mut inner = task_driver.state().lock();
                if inner.generation != generation {
                    return;
                }
                if inner.pulls.remove(&task_ref).is_some() {
                    inner.dirty_slices.pulls = true;
                }
                inner.pull_tasks.remove(&task_ref);
            }
            changed_with(&task_driver);
            match failure {
                Some(e) if e.is_connection_lost() => {
                    connection_lost_with(&task_driver, generation, &e);
                }
                Some(e) => {
                    tracing::warn!(reference = %task_ref, error = %e, "image pull failed");
                    task_driver.emit_notice(Notice::PullFailed {
                        reference: task_ref.clone(),
                        error: e.to_string(),
                    });
                }
                None => {
                    task_driver.emit_notice(Notice::Pulled(task_ref.clone()));
                    task_driver.state().lock().disk_notify.notify_one();
                    refresh_with(&task_driver, ResourceKind::Image);
                }
            }
        });
        inner.pull_tasks.insert(reference, task);
    }
    changed_with(driver);
    Ok(())
}

pub fn compose(
    app: &Backend,
    labels: engine::ComposeLabels,
    command: engine::ComposeCommand,
) -> Result<(), UiError> {
    compose_with(&NativeDriver(app.clone()), labels, command)
}

fn compose_with<D: Driver>(
    driver: &D,
    labels: engine::ComposeLabels,
    command: engine::ComposeCommand,
) -> Result<(), UiError> {
    let key = format!("compose:{}", labels.project);
    {
        let mut inner = driver.state().lock();
        if !inner.pending.insert(key.clone()) {
            return Err(busy(&format!("A compose command for {}", labels.project)));
        }
        inner.dirty_slices.pending = true;
        let generation = inner.generation;
        let target = inner.target.clone();
        let project = labels.project.clone();
        inner.next_compose_seq += 1;
        let seq = inner.next_compose_seq;
        inner.compose_runs.insert(
            project.clone(),
            InternalComposeRun {
                project: project.clone(),
                command: format!("docker compose {}", command.args().join(" ")),
                lines: VecDeque::new(),
                result: RunResult::Running,
                seq,
            },
        );
        inner.dirty_slices.compose_runs = true;
        inner.evict_finished_compose_runs();
        let task_driver = driver.clone();
        let task_project = project.clone();
        let task = driver.spawn(async move {
            let mut rx = task_driver.run_compose(&target, &labels, command);
            while let Some(out) = rx.next().await {
                let finished = {
                    let mut inner = task_driver.state().lock();
                    if inner.generation != generation {
                        return;
                    }
                    let Some(run) = inner.compose_runs.get_mut(&task_project) else {
                        return;
                    };
                    match out {
                        ComposeOutput::Line(l) => {
                            if run.lines.len() >= COMPOSE_LINES {
                                run.lines.pop_front();
                            }
                            run.lines.push_back(l);
                            inner.dirty_slices.compose_runs = true;
                            None
                        }
                        ComposeOutput::Exit(r) => {
                            run.result = match &r {
                                Ok(()) => RunResult::Ok,
                                Err(e) => RunResult::Failed { message: e.clone() },
                            };
                            inner.dirty_slices.compose_runs = true;
                            if inner.pending.remove(&key) {
                                inner.dirty_slices.pending = true;
                            }
                            inner.compose_tasks.remove(&task_project);
                            inner.evict_finished_compose_runs();
                            Some(r)
                        }
                    }
                };
                changed_with(&task_driver);
                if let Some(r) = finished {
                    match r {
                        Ok(()) => task_driver.emit_notice(Notice::ComposeDone {
                            command: command.label().to_owned(),
                            project: task_project.clone(),
                        }),
                        Err(e) => {
                            tracing::warn!(project = %task_project, error = %e, "compose command failed");
                            task_driver.emit_notice(Notice::Error(e));
                        }
                    }
                    refresh_all_with(&task_driver);
                    return;
                }
            }
            let cleaned = {
                let mut inner = task_driver.state().lock();
                if inner.generation != generation {
                    false
                } else {
                    inner.compose_tasks.remove(&task_project);
                    let was_pending = inner.pending.remove(&key);
                    if was_pending {
                        inner.dirty_slices.pending = true;
                    }
                    if was_pending
                        && let Some(run) = inner.compose_runs.get_mut(&task_project)
                        && matches!(run.result, RunResult::Running)
                    {
                        run.result = RunResult::Failed {
                            message: "Compose command ended unexpectedly.".into(),
                        };
                        inner.dirty_slices.compose_runs = true;
                    }
                    inner.evict_finished_compose_runs();
                    was_pending
                }
            };
            if cleaned {
                changed_with(&task_driver);
            }
        });
        inner.compose_tasks.insert(project, task);
    }
    changed_with(driver);
    Ok(())
}

pub fn cancel_compose(app: &Backend, project: &str) -> Result<(), UiError> {
    cancel_compose_with(&NativeDriver(app.clone()), project)
}

fn cancel_compose_with<D: Driver>(driver: &D, project: &str) -> Result<(), UiError> {
    let key = format!("compose:{project}");
    let task = {
        let mut inner = driver.state().lock();
        let Some(task) = inner.compose_tasks.remove(project) else {
            return Err(UiError {
                kind: dto::UiErrorKind::NotFound,
                message: format!("No running compose command for {project}."),
            });
        };
        if inner.pending.remove(&key) {
            inner.dirty_slices.pending = true;
        }
        if let Some(run) = inner.compose_runs.get_mut(project) {
            run.result = RunResult::Failed {
                message: "Cancelled.".into(),
            };
            inner.dirty_slices.compose_runs = true;
        }
        inner.evict_finished_compose_runs();
        task
    };
    task.abort();
    changed_with(driver);
    refresh_all_with(driver);
    Ok(())
}

pub fn dismiss_compose(app: &Backend, project: &str) {
    dismiss_compose_with(&NativeDriver(app.clone()), project);
}

fn dismiss_compose_with<D: Driver>(driver: &D, project: &str) {
    let removed = {
        let mut inner = driver.state().lock();
        let can_remove = inner
            .compose_runs
            .get(project)
            .is_some_and(|r| !matches!(r.result, RunResult::Running));
        if can_remove {
            inner.compose_runs.remove(project);
            inner.dirty_slices.compose_runs = true;
            true
        } else {
            false
        }
    };
    if removed {
        changed_with(driver);
    }
}

pub fn set_compose_available(app: &Backend, available: bool) {
    NativeDriver(app.clone()).state().lock().compose_available = Some(available);
    changed_with(&NativeDriver(app.clone()));
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::dto::NoticeKind;

    fn test_target(host: &str) -> ConnectTarget {
        ConnectTarget {
            host: host.into(),
            source: "test".into(),
            context: None,
            tls: None,
        }
    }

    fn inner() -> Inner {
        AppState::new(test_target("unix:///x"))
            .0
            .into_inner()
            .unwrap()
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let seq: Vec<u32> = (0..8).map(backoff_secs).collect();
        assert_eq!(seq, vec![1, 2, 4, 8, 16, 30, 30, 30]);
    }

    #[test]
    fn compose_labels_resolve_only_while_project_containers_exist() {
        let state = AppState::new(test_target("unix:///x"));
        assert!(state.compose_labels("shop").is_none());

        state.lock().containers.push(engine::ContainerSummary {
            id: "shop-web".into(),
            name: "shop-web-1".into(),
            image: "nginx".into(),
            state: engine::ContainerState::Exited,
            status: String::new(),
            created: 0,
            ports: Vec::new(),
            compose: Some(engine::ComposeLabels {
                project: "shop".into(),
                service: "web".into(),
                working_dir: Some("/src/shop".into()),
                config_files: vec!["/src/shop/compose.yaml".into()],
            }),
            networks: Vec::new(),
            volumes: Vec::new(),
        });
        let labels = state.compose_labels("shop").unwrap();
        assert_eq!(labels.working_dir.as_deref(), Some("/src/shop"));

        state.lock().containers.clear();
        assert!(state.compose_labels("shop").is_none());
    }

    #[test]
    fn missing_socket_retries_with_growing_backoff() {
        let mut s = inner();
        let e = EngineError::SocketMissing("/x".into());
        assert_eq!(s.apply_error(&e), Next::RetryIn(1));
        assert_eq!(s.apply_error(&e), Next::RetryIn(2));
        assert!(matches!(s.conn, ConnState::Unreachable { retry_in: 2, .. }));
    }

    #[test]
    fn losing_an_established_connection_is_reconnecting_and_keeps_lists() {
        let mut s = inner();
        s.conn = ConnState::Connected {
            info: dto::EngineInfo {
                version: "1".into(),
                api_version: "1".into(),
                os: "l".into(),
                arch: "a".into(),
            },
        };
        s.networks.push(engine::NetworkSummary {
            id: "n".into(),
            name: "bridge".into(),
            driver: "bridge".into(),
            scope: "local".into(),
            internal: false,
        });
        s.pending.insert("x".into());
        assert_eq!(
            s.apply_error(&EngineError::Transport("closed".into())),
            Next::RetryIn(1)
        );
        assert!(matches!(s.conn, ConnState::Reconnecting { .. }));
        assert_eq!(s.networks.len(), 1);
        assert!(s.pending.is_empty());
    }

    #[test]
    fn permission_denied_does_not_retry() {
        let mut s = inner();
        assert_eq!(
            s.apply_error(&EngineError::PermissionDenied("/x".into())),
            Next::Done
        );
        assert!(matches!(s.conn, ConnState::PermissionDenied { .. }));
        assert_eq!(s.attempt, 0);
    }

    #[test]
    fn pull_progress_tracks_layers() {
        let mut p = PullState::default();
        let ev = |layer: &str, status: &str, cur: Option<i64>, total: Option<i64>| PullProgress {
            layer: Some(layer.into()),
            status: status.into(),
            current: cur,
            total,
        };
        p.update(ev("a", "Downloading", Some(50), Some(100)));
        p.update(ev("b", "Downloading", Some(0), Some(100)));
        assert_eq!(p.fraction(), Some(0.25));
        p.update(ev("b", "Download complete", None, None));
        assert_eq!(p.fraction(), Some(0.75));
        assert_eq!(p.status, "b: Download complete");
    }

    type PullStreamFactory =
        Box<dyn FnMut() -> BoxStream<'static, Result<PullProgress, EngineError>> + Send>;
    type ComposeStreamFactory = Box<dyn FnMut() -> BoxStream<'static, ComposeOutput> + Send>;

    type ConnectFactory = Box<
        dyn FnMut(
                ConnectTarget,
            ) -> BoxFuture<'static, Result<(FakeClient, EngineInfo), EngineError>>
            + Send,
    >;

    #[derive(Default)]
    struct FakeBackend {
        containers: Mutex<Vec<engine::ContainerSummary>>,
        container_calls: AtomicUsize,
        images: Mutex<Vec<engine::ImageSummary>>,
        image_calls: AtomicUsize,
        capacity: Mutex<engine::SystemCapacity>,
        usage_samples: Mutex<VecDeque<engine::ContainerUsageSample>>,
        disk: Mutex<Option<engine::DiskUsage>>,
        disk_calls: AtomicUsize,
        network_gates: Mutex<VecDeque<tokio::sync::oneshot::Receiver<Vec<engine::NetworkSummary>>>>,
        events_rx: Mutex<
            Option<
                futures::channel::mpsc::UnboundedReceiver<Result<engine::EngineEvent, EngineError>>,
            >,
        >,
        pull_factory: Mutex<Option<PullStreamFactory>>,
        compose_factory: Mutex<Option<ComposeStreamFactory>>,
        connect_factory: Mutex<Option<ConnectFactory>>,
        daemon_gate:
            Mutex<Option<tokio::sync::oneshot::Receiver<Result<&'static str, EngineError>>>>,
        /// Calls (`"capacity"`, `"images"`, `"usage"`, `"disk"`) that fail with this
        /// error instead of answering.
        failures: Mutex<HashMap<&'static str, EngineError>>,
    }

    impl FakeBackend {
        fn failure(&self, call: &str) -> Option<EngineError> {
            self.failures.lock().unwrap().get(call).cloned()
        }
    }

    #[derive(Clone)]
    struct FakeClient(Arc<FakeBackend>);

    impl ClientOps for FakeClient {
        fn events(&self) -> BoxStream<'static, Result<engine::EngineEvent, EngineError>> {
            match self.0.events_rx.lock().unwrap().take() {
                Some(rx) => Box::pin(rx),
                None => Box::pin(futures::stream::pending()),
            }
        }

        fn containers(
            &self,
            _all: bool,
        ) -> BoxFuture<'static, Result<Vec<engine::ContainerSummary>, EngineError>> {
            self.0.container_calls.fetch_add(1, Ordering::SeqCst);
            let list = self.0.containers.lock().unwrap().clone();
            Box::pin(async move { Ok(list) })
        }

        fn images(&self) -> BoxFuture<'static, Result<Vec<engine::ImageSummary>, EngineError>> {
            self.0.image_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(e) = self.0.failure("images") {
                return Box::pin(async move { Err(e) });
            }
            let imgs = self.0.images.lock().unwrap().clone();
            Box::pin(async move { Ok(imgs) })
        }

        fn volumes(&self) -> BoxFuture<'static, Result<Vec<engine::VolumeSummary>, EngineError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn networks(&self) -> BoxFuture<'static, Result<Vec<engine::NetworkSummary>, EngineError>> {
            let gate = self.0.network_gates.lock().unwrap().pop_front();
            Box::pin(async move {
                match gate {
                    Some(rx) => Ok(rx.await.unwrap_or_default()),
                    None => Ok(Vec::new()),
                }
            })
        }

        fn pull(&self, _reference: &str) -> BoxStream<'static, Result<PullProgress, EngineError>> {
            if let Some(f) = self.0.pull_factory.lock().unwrap().as_mut() {
                return f();
            }
            Box::pin(futures::stream::empty())
        }

        fn system_capacity(
            &self,
        ) -> BoxFuture<'static, Result<engine::SystemCapacity, EngineError>> {
            if let Some(e) = self.0.failure("capacity") {
                return Box::pin(async move { Err(e) });
            }
            let cap = *self.0.capacity.lock().unwrap();
            Box::pin(async move { Ok(cap) })
        }

        fn container_usage(
            &self,
            _ids: Vec<String>,
        ) -> BoxFuture<'static, Result<engine::ContainerUsageSample, EngineError>> {
            if let Some(e) = self.0.failure("usage") {
                return Box::pin(async move { Err(e) });
            }
            let sample = self
                .0
                .usage_samples
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default();
            Box::pin(async move { Ok(sample) })
        }

        fn disk_usage(&self) -> BoxFuture<'static, Result<Option<engine::DiskUsage>, EngineError>> {
            self.0.disk_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(e) = self.0.failure("disk") {
                return Box::pin(async move { Err(e) });
            }
            let disk = self.0.disk.lock().unwrap().clone();
            Box::pin(async move { Ok(disk) })
        }
    }

    struct FakeDriverInner {
        backend: Arc<FakeBackend>,
        state: AppState<FakeClient>,
        cleared_streams: AtomicUsize,
        usage_poll_ms: AtomicUsize,
        first_cpu_ms: AtomicUsize,
        patches: Mutex<Vec<StatePatch>>,
        notices: Mutex<Vec<Notice>>,
        exits: Mutex<Vec<dto::ContainerExit>>,
    }

    #[derive(Clone)]
    struct FakeDriver(Arc<FakeDriverInner>);

    impl FakeDriver {
        fn new(backend: Arc<FakeBackend>) -> Self {
            Self(Arc::new(FakeDriverInner {
                backend,
                state: AppState::with_client(test_target("unix:///fake")),
                cleared_streams: AtomicUsize::new(0),
                usage_poll_ms: AtomicUsize::new(2000),
                first_cpu_ms: AtomicUsize::new(500),
                patches: Mutex::new(Vec::new()),
                notices: Mutex::new(Vec::new()),
                exits: Mutex::new(Vec::new()),
            }))
        }
    }

    impl Driver for FakeDriver {
        type Client = FakeClient;

        fn state(&self) -> &AppState<FakeClient> {
            &self.0.state
        }

        fn connect(
            &self,
            target: ConnectTarget,
        ) -> BoxFuture<'static, Result<(FakeClient, EngineInfo), EngineError>> {
            if let Some(f) = self.0.backend.connect_factory.lock().unwrap().as_mut() {
                return f(target);
            }
            let backend = self.0.backend.clone();
            Box::pin(async move {
                Ok((
                    FakeClient(backend),
                    EngineInfo {
                        version: "27.0".into(),
                        api_version: "1.46".into(),
                        os: "linux".into(),
                        arch: "amd64".into(),
                        host: target.host,
                    },
                ))
            })
        }

        fn run_compose(
            &self,
            _target: &ConnectTarget,
            _labels: &engine::ComposeLabels,
            _command: engine::ComposeCommand,
        ) -> BoxStream<'static, ComposeOutput> {
            if let Some(f) = self.0.backend.compose_factory.lock().unwrap().as_mut() {
                return f();
            }
            Box::pin(futures::stream::empty())
        }

        fn start_daemon(
            &self,
            _target: ConnectTarget,
        ) -> BoxFuture<'static, Result<&'static str, EngineError>> {
            let gate = self.0.backend.daemon_gate.lock().unwrap().take();
            Box::pin(async move {
                match gate {
                    Some(rx) => rx.await.unwrap_or(Ok("Docker")),
                    None => Ok("Docker"),
                }
            })
        }

        fn clear_streams(&self) {
            self.0.cleared_streams.fetch_add(1, Ordering::SeqCst);
        }

        fn emit_patch(&self, patch: StatePatch) {
            self.0.patches.lock().unwrap().push(patch);
        }

        fn emit_notice(&self, notice: Notice) {
            self.0.notices.lock().unwrap().push(notice);
        }

        fn emit_exit(&self, exit: dto::ContainerExit) {
            self.0.exits.lock().unwrap().push(exit);
        }

        fn spawn<F>(&self, fut: F) -> TaskHandle
        where
            F: Future<Output = ()> + Send + 'static,
        {
            TaskHandle(tokio::spawn(fut))
        }

        fn debounce_duration(&self) -> Duration {
            Duration::from_millis(5)
        }

        fn emit_duration(&self) -> Duration {
            Duration::from_millis(5)
        }

        fn usage_poll_duration(&self) -> Duration {
            Duration::from_millis(self.0.usage_poll_ms.load(Ordering::SeqCst) as u64)
        }

        fn first_cpu_sample_duration(&self) -> Duration {
            Duration::from_millis(self.0.first_cpu_ms.load(Ordering::SeqCst) as u64)
        }
    }

    async fn connected(backend: Arc<FakeBackend>) -> FakeDriver {
        let driver = FakeDriver::new(backend);
        connect_with(&driver, None);
        for _ in 0..100 {
            if matches!(driver.state().snapshot().conn, ConnState::Connected { .. })
                && driver.state().lock().disk_task.is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(matches!(
            driver.state().snapshot().conn,
            ConnState::Connected { .. }
        ));
        driver
    }

    async fn settle_until(done: impl FnMut() -> bool) {
        settle_until_within(Duration::from_millis(500), done).await;
    }

    async fn settle_until_within(limit: Duration, mut done: impl FnMut() -> bool) {
        let start = std::time::Instant::now();
        while start.elapsed() < limit {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("condition not reached within {limit:?}");
    }

    fn is_pending(driver: &FakeDriver, key: &str) -> bool {
        driver.state().snapshot().pending.iter().any(|p| p == key)
    }

    const LISTS: &[ResourceKind] = &[
        ResourceKind::Container,
        ResourceKind::Image,
        ResourceKind::Volume,
        ResourceKind::Network,
    ];

    #[tokio::test]
    async fn an_operation_refuses_a_second_start_on_the_same_key_until_it_ends() {
        let backend = Arc::new(FakeBackend::default());
        let driver = connected(backend).await;
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let ran = Arc::new(AtomicUsize::new(0));
        let on_ok = {
            let ran = ran.clone();
            move |_: &FakeDriver, ()| {
                ran.fetch_add(1, Ordering::SeqCst);
            }
        };
        let op = async move {
            let _ = done_rx.await;
            Ok(())
        };
        run_op_with(
            &driver,
            "remove:a".into(),
            &[],
            Op::RemoveVolume("a".into()),
            move || op,
            on_ok,
        )
        .unwrap();
        assert!(is_pending(&driver, "remove:a"));

        // The refused second click sends nothing: its request is never made.
        let started = AtomicUsize::new(0);
        let again = run_op_with(
            &driver,
            "remove:a".into(),
            &[],
            Op::RemoveVolume("a".into()),
            || {
                started.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            },
            |_: &FakeDriver, ()| {},
        );
        assert_eq!(again.unwrap_err().kind, dto::UiErrorKind::Conflict);
        assert_eq!(started.load(Ordering::SeqCst), 0);
        // Another item is not blocked.
        run_op_with(
            &driver,
            "remove:b".into(),
            &[],
            Op::RemoveVolume("b".into()),
            || async { Ok(()) },
            |_: &FakeDriver, ()| {},
        )
        .unwrap();

        done_tx.send(()).unwrap();
        settle_until(|| !is_pending(&driver, "remove:a")).await;
        settle_until(|| ran.load(Ordering::SeqCst) == 1).await;
        assert!(driver.state().snapshot().pending.is_empty());
    }

    #[tokio::test]
    async fn a_finished_operation_re_lists_each_kind_once_and_re_reads_disk_usage() {
        let backend = Arc::new(FakeBackend::default());
        let driver = connected(backend.clone()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (containers, images, disk) = (
            backend.container_calls.load(Ordering::SeqCst),
            backend.image_calls.load(Ordering::SeqCst),
            backend.disk_calls.load(Ordering::SeqCst),
        );

        run_op_with(
            &driver,
            "prune:x".into(),
            LISTS,
            Op::Prune(dto::PruneKind::Images),
            || async { Ok(()) },
            |_: &FakeDriver, ()| {},
        )
        .unwrap();
        settle_until(|| backend.disk_calls.load(Ordering::SeqCst) > disk).await;
        settle_until(|| backend.image_calls.load(Ordering::SeqCst) > images).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(backend.image_calls.load(Ordering::SeqCst), images + 1);
        assert_eq!(
            backend.container_calls.load(Ordering::SeqCst),
            containers + 1
        );
        assert!(driver.0.notices.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failed_operation_says_what_failed_and_frees_its_key() {
        let backend = Arc::new(FakeBackend::default());
        let driver = connected(backend.clone()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let images = backend.image_calls.load(Ordering::SeqCst);

        run_op_with(
            &driver,
            "remove:a".into(),
            LISTS,
            Op::RemoveVolume("a".into()),
            || async { Err::<(), _>(EngineError::Local("in use".into())) },
            |_: &FakeDriver, ()| panic!("on_ok runs only on success"),
        )
        .unwrap();
        settle_until(|| !driver.0.notices.lock().unwrap().is_empty()).await;
        let notices = driver.0.notices.lock().unwrap().clone();
        assert!(
            matches!(&notices[0], Notice::Failed { op: Op::RemoveVolume(n), error } if n == "a" && error == "in use"),
            "{notices:?}"
        );
        assert!(driver.state().snapshot().pending.is_empty());
        // Nothing changed, so nothing is re-listed and the connection stays up.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(backend.image_calls.load(Ordering::SeqCst), images);
        assert!(matches!(
            driver.state().snapshot().conn,
            ConnState::Connected { .. }
        ));
    }

    #[tokio::test]
    async fn an_operation_that_loses_the_connection_reconnects_instead_of_a_notice() {
        let backend = Arc::new(FakeBackend::default());
        let driver = connected(backend).await;
        run_op_with(
            &driver,
            "stop:a".into(),
            &[],
            Op::Container(dto::ContainerAction::Stop),
            || async { Err::<(), _>(EngineError::Transport("closed".into())) },
            |_: &FakeDriver, ()| {},
        )
        .unwrap();
        settle_until(|| {
            !matches!(driver.state().snapshot().conn, ConnState::Connected { .. })
                || driver.0.cleared_streams.load(Ordering::SeqCst) > 0
        })
        .await;
        assert!(driver.0.notices.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_operation_keeps_its_key_across_a_reconnect_and_reports_how_it_ended() {
        let backend = Arc::new(FakeBackend::default());
        let driver = connected(backend).await;
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        run_op_with(
            &driver,
            "stop:a".into(),
            &[],
            Op::Container(dto::ContainerAction::Stop),
            move || async move {
                let _ = done_rx.await;
                Err::<(), _>(EngineError::Local("gone".into()))
            },
            |_: &FakeDriver, ()| {},
        )
        .unwrap();
        connect_with(&driver, None);
        settle_until(|| is_connected(&driver)).await;

        // The request is still running, so a second one on the same item is refused.
        assert!(is_pending(&driver, "stop:a"));
        let again = run_op_with(
            &driver,
            "stop:a".into(),
            &[],
            Op::Container(dto::ContainerAction::Stop),
            || async { Ok(()) },
            |_: &FakeDriver, ()| {},
        );
        assert_eq!(again.unwrap_err().kind, dto::UiErrorKind::Conflict);

        done_tx.send(()).unwrap();
        settle_until(|| !is_pending(&driver, "stop:a")).await;
        settle_until(|| !driver.0.notices.lock().unwrap().is_empty()).await;
        let notices = driver.0.notices.lock().unwrap().clone();
        assert!(
            matches!(
                &notices[..],
                [Notice::Failed { op: Op::Container(dto::ContainerAction::Stop), error }] if error.contains("gone")
            ),
            "{notices:?}"
        );
    }

    #[tokio::test]
    async fn a_timed_out_operation_says_so_and_keeps_the_connection() {
        let backend = Arc::new(FakeBackend::default());
        let driver = connected(backend).await;
        let cleared = driver.0.cleared_streams.load(Ordering::SeqCst);
        run_op_with(
            &driver,
            "remove:a".into(),
            &[],
            Op::RemoveVolume("a".into()),
            || async { Err::<(), _>(EngineError::Timeout("timed out after 120s".into())) },
            |_: &FakeDriver, ()| {},
        )
        .unwrap();
        settle_until(|| !driver.0.notices.lock().unwrap().is_empty()).await;
        let notices = driver.0.notices.lock().unwrap().clone();
        assert!(
            matches!(&notices[..], [Notice::Failed { error, .. }] if error.contains("timed out")),
            "{notices:?}"
        );
        assert!(is_connected(&driver));
        assert_eq!(driver.0.cleared_streams.load(Ordering::SeqCst), cleared);
    }

    /// A driver that has not connected, for a daemon that `label` can start.
    fn down_daemon(label: Option<&'static str>) -> (FakeDriver, Arc<FakeBackend>) {
        let backend = Arc::new(FakeBackend::default());
        let driver = FakeDriver::new(backend.clone());
        driver.state().lock().daemon_startable = label;
        (driver, backend)
    }

    fn is_connected(driver: &FakeDriver) -> bool {
        matches!(driver.state().snapshot().conn, ConnState::Connected { .. })
    }

    #[tokio::test]
    async fn starting_a_daemon_nobody_knows_how_to_start_is_refused() {
        let (driver, _) = down_daemon(None);
        let e = start_daemon_with(&driver).unwrap_err();
        assert_eq!(e.kind, dto::UiErrorKind::Local);
        assert!(driver.state().lock().daemon_starting.is_none());
    }

    #[tokio::test]
    async fn a_started_daemon_is_reconnected_at_once_and_a_second_start_waits() {
        let (driver, backend) = down_daemon(Some("Docker"));
        let (tx, rx) = tokio::sync::oneshot::channel();
        *backend.daemon_gate.lock().unwrap() = Some(rx);

        start_daemon_with(&driver).unwrap();
        assert_eq!(
            start_daemon_with(&driver).unwrap_err().kind,
            dto::UiErrorKind::Conflict
        );
        assert!(!is_connected(&driver));

        tx.send(Ok("Docker")).unwrap();
        settle_until(|| is_connected(&driver)).await;
        assert!(driver.state().lock().daemon_starting.is_none());
        let notices = driver.0.notices.lock().unwrap().clone();
        assert!(
            matches!(&notices[..], [Notice::DaemonStarted(l)] if l == "Docker"),
            "{notices:?}"
        );
    }

    #[tokio::test]
    async fn a_daemon_that_fails_to_start_says_why_and_does_not_reconnect() {
        let (driver, backend) = down_daemon(Some("Docker"));
        let (tx, rx) = tokio::sync::oneshot::channel();
        *backend.daemon_gate.lock().unwrap() = Some(rx);
        start_daemon_with(&driver).unwrap();
        tx.send(Err(EngineError::Local("systemctl failed".into())))
            .unwrap();
        settle_until(|| driver.state().lock().daemon_starting.is_none()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!is_connected(&driver));
        let notices = driver.0.notices.lock().unwrap().clone();
        assert!(
            matches!(&notices[..], [Notice::Error(m)] if m.contains("systemctl failed")),
            "{notices:?}"
        );
    }

    #[tokio::test]
    async fn a_daemon_started_for_a_target_left_behind_does_not_reconnect() {
        let (driver, backend) = down_daemon(Some("Docker"));
        let (tx, rx) = tokio::sync::oneshot::channel();
        *backend.daemon_gate.lock().unwrap() = Some(rx);
        start_daemon_with(&driver).unwrap();
        driver.state().lock().target = test_target("unix:///other");
        tx.send(Ok("Docker")).unwrap();
        settle_until(|| driver.state().lock().daemon_starting.is_none()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!is_connected(&driver));
    }

    #[tokio::test]
    async fn a_retry_connects_only_if_nothing_reconnected_in_the_meantime() {
        // Backoff is in whole seconds, so each case waits one.
        let (driver, _) = down_daemon(None);
        let generation = driver.state().lock().generation;
        schedule_retry_with(&driver, generation, 1);
        settle_until_within(Duration::from_secs(3), || is_connected(&driver)).await;

        let (stale, _) = down_daemon(None);
        let generation = stale.state().lock().generation;
        schedule_retry_with(&stale, generation, 1);
        // A switch of target (or a manual reconnect) starts a new generation.
        stale.state().lock().generation += 1;
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert!(!is_connected(&stale));
    }

    #[tokio::test]
    async fn a_lost_connection_from_an_older_generation_is_ignored() {
        let backend = Arc::new(FakeBackend::default());
        let driver = connected(backend).await;
        let generation = driver.state().lock().generation;
        let cleared = driver.0.cleared_streams.load(Ordering::SeqCst);
        connection_lost_with(
            &driver,
            generation.wrapping_sub(1),
            &EngineError::Transport("closed".into()),
        );
        assert!(is_connected(&driver));
        assert_eq!(driver.0.cleared_streams.load(Ordering::SeqCst), cleared);

        connection_lost_with(
            &driver,
            generation,
            &EngineError::Transport("closed".into()),
        );
        assert!(!is_connected(&driver));
        assert_eq!(driver.0.cleared_streams.load(Ordering::SeqCst), cleared + 1);
    }

    #[tokio::test]
    async fn relisting_an_unchanged_list_emits_no_patch() {
        let backend = Arc::new(FakeBackend::default());
        *backend.images.lock().unwrap() = vec![engine::ImageSummary {
            id: "sha256:1111111111111111".into(),
            tags: vec!["alpine:3.20".into()],
            size: 100,
            created: 1,
            containers: 0,
        }];
        let driver = FakeDriver::new(backend.clone());
        connect_with(&driver, None);
        for _ in 0..40 {
            if !driver.state().snapshot().images.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        driver.0.patches.lock().unwrap().clear();
        let revision = driver.state().snapshot().revision;
        let calls = backend.image_calls.load(Ordering::SeqCst);

        refresh_with(&driver, ResourceKind::Image);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(backend.image_calls.load(Ordering::SeqCst), calls + 1);
        assert_eq!(driver.state().snapshot().revision, revision);
        assert!(driver.0.patches.lock().unwrap().is_empty());

        // A list that did change is still pushed, with only its slice.
        backend.images.lock().unwrap()[0].containers = 1;
        refresh_with(&driver, ResourceKind::Image);
        tokio::time::sleep(Duration::from_millis(30)).await;
        let snap = driver.state().snapshot();
        assert_eq!(snap.images[0].containers, 1.0);
        assert!(snap.revision != revision);
        let patches = driver.0.patches.lock().unwrap();
        assert_eq!(patches.len(), 1);
        let images = patches[0]
            .images
            .as_ref()
            .expect("the changed list is sent");
        assert_eq!(images[0].containers, 1.0);
        assert!(patches[0].containers.is_none() && patches[0].volumes.is_none());
    }

    #[tokio::test]
    async fn container_create_event_marks_images_dirty_and_refreshes_usage() {
        // A-002 / R-12
        let backend = Arc::new(FakeBackend::default());
        *backend.images.lock().unwrap() = vec![engine::ImageSummary {
            id: "sha256:1111111111111111".into(),
            tags: vec!["alpine:3.20".into()],
            size: 100,
            created: 1,
            containers: 0,
        }];
        let (ev_tx, ev_rx) = futures::channel::mpsc::unbounded();
        *backend.events_rx.lock().unwrap() = Some(ev_rx);
        let driver = FakeDriver::new(backend.clone());
        connect_with(&driver, None);

        for _ in 0..20 {
            if backend.image_calls.load(Ordering::SeqCst) >= 1
                && !driver.state().snapshot().images.is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(driver.state().snapshot().images[0].containers, 0.0);

        backend.images.lock().unwrap()[0].containers = 1;
        ev_tx
            .unbounded_send(Ok(engine::EngineEvent {
                kind: ResourceKind::Container,
                action: "create".into(),
                id: "c1".into(),
                name: None,
                exit_code: None,
            }))
            .unwrap();

        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(driver.state().snapshot().images[0].containers, 1.0);
    }

    #[tokio::test]
    async fn older_concurrent_refresh_does_not_overwrite_newer_list() {
        // A-002 / R-12
        let backend = Arc::new(FakeBackend::default());
        let (init_tx, init_rx) = tokio::sync::oneshot::channel();
        let (slow_tx, slow_rx) = tokio::sync::oneshot::channel();
        let (fast_tx, fast_rx) = tokio::sync::oneshot::channel();
        init_tx.send(Vec::new()).unwrap();
        backend
            .network_gates
            .lock()
            .unwrap()
            .extend([init_rx, slow_rx, fast_rx]);

        let driver = FakeDriver::new(backend);
        connect_with(&driver, None);
        for _ in 0..20 {
            if matches!(driver.state().snapshot().conn, ConnState::Connected { .. }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;

        let net = |name: &str| engine::NetworkSummary {
            id: name.into(),
            name: name.into(),
            driver: "bridge".into(),
            scope: "local".into(),
            internal: false,
        };

        refresh_with(&driver, ResourceKind::Network);
        refresh_with(&driver, ResourceKind::Network);

        fast_tx.send(vec![net("newer")]).unwrap();
        for _ in 0..20 {
            if driver
                .state()
                .snapshot()
                .networks
                .first()
                .is_some_and(|n| n.name == "newer")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let rev_after_newer = driver.state().snapshot().revision;

        slow_tx.send(vec![net("older")]).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let snap = driver.state().snapshot();
        assert_eq!(snap.networks.len(), 1);
        assert_eq!(snap.networks[0].name, "newer");
        assert_eq!(snap.revision, rev_after_newer);
    }

    #[tokio::test]
    async fn reconnect_clears_inflight_pulls_and_aborts_compose_across_generations() {
        // A-001, A-004 / R-10, R-11
        let backend = Arc::new(FakeBackend::default());
        let pull_token = Arc::new(());
        let held_pull = pull_token.clone();
        *backend.pull_factory.lock().unwrap() = Some(Box::new(move || {
            let keep = held_pull.clone();
            Box::pin(futures::stream::poll_fn(move |_| {
                let _ = &keep;
                std::task::Poll::Pending
            }))
        }));

        let compose_token = Arc::new(());
        let held_compose = compose_token.clone();
        *backend.compose_factory.lock().unwrap() = Some(Box::new(move || {
            let keep = held_compose.clone();
            Box::pin(
                futures::stream::once(async { ComposeOutput::Line("starting".into()) }).chain(
                    futures::stream::poll_fn(move |_| {
                        let _ = &keep;
                        std::task::Poll::Pending
                    }),
                ),
            )
        }));

        let driver = FakeDriver::new(backend.clone());
        connect_with(&driver, None);
        for _ in 0..20 {
            if matches!(driver.state().snapshot().conn, ConnState::Connected { .. }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let client = driver.state().engine().unwrap();
        pull_with(&driver, client, "alpine:3.20".into()).unwrap();
        compose_with(
            &driver,
            engine::ComposeLabels {
                project: "demo".into(),
                service: "web".into(),
                working_dir: Some("/tmp".into()),
                config_files: Vec::new(),
            },
            engine::ComposeCommand::Up,
        )
        .unwrap();

        tokio::time::sleep(Duration::from_millis(15)).await;
        assert_eq!(driver.state().snapshot().pulls.len(), 1);

        *backend.pull_factory.lock().unwrap() =
            Some(Box::new(|| Box::pin(futures::stream::empty())));
        *backend.compose_factory.lock().unwrap() = None;
        assert_eq!(Arc::strong_count(&pull_token), 2);
        assert_eq!(Arc::strong_count(&compose_token), 2);
        let cleared_before = driver.0.cleared_streams.load(Ordering::SeqCst);

        connect_with(&driver, None);
        for _ in 0..20 {
            if matches!(driver.state().snapshot().conn, ConnState::Connected { .. })
                && driver.state().lock().generation == 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;

        assert!(
            driver.0.cleared_streams.load(Ordering::SeqCst) > cleared_before,
            "reconnect should clear active UI streams"
        );
        let snap = driver.state().snapshot();
        assert!(
            snap.pulls.is_empty(),
            "stale pull entry leaked across reconnect"
        );
        assert!(
            matches!(snap.compose_runs[0].result, RunResult::Failed { .. }),
            "interrupted compose run remained Running: {:?}",
            snap.compose_runs
        );
        assert_eq!(Arc::strong_count(&pull_token), 1, "old pull stream leaked");
        assert_eq!(
            Arc::strong_count(&compose_token),
            1,
            "old compose stream leaked"
        );

        let client2 = driver.state().engine().unwrap();
        pull_with(&driver, client2, "alpine:3.20".into())
            .expect("re-pulling same image after reconnect must not conflict");
    }

    #[tokio::test]
    async fn pull_progress_patches_omit_unchanged_resource_lists() {
        // P-003: high-rate pull progress patches serialize None for unchanged lists.
        let backend = Arc::new(FakeBackend::default());
        let (pull_tx, pull_rx) = futures::channel::mpsc::unbounded();
        let pull_rx = Arc::new(Mutex::new(Some(pull_rx)));
        *backend.pull_factory.lock().unwrap() = Some(Box::new(move || {
            Box::pin(pull_rx.lock().unwrap().take().unwrap())
        }));

        let driver = FakeDriver::new(backend);
        connect_with(&driver, None);
        tokio::time::sleep(Duration::from_millis(25)).await;

        // Positive control: initial list refresh emitted patches containing containers/images.
        assert!(
            driver
                .0
                .patches
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.containers.is_some()),
            "initial refresh must populate containers slice"
        );
        driver.0.patches.lock().unwrap().clear();

        let client = driver.state().engine().unwrap();
        pull_with(&driver, client, "nginx:1.27".into()).unwrap();
        pull_tx
            .unbounded_send(Ok(PullProgress {
                layer: Some("l1".into()),
                status: "Downloading".into(),
                current: Some(50),
                total: Some(100),
            }))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let patches = driver.0.patches.lock().unwrap().clone();
        assert!(!patches.is_empty());
        for p in &patches {
            assert!(p.pulls.is_some());
            assert!(
                p.containers.is_none(),
                "unchanged containers cloned in patch: {p:?}"
            );
            assert!(
                p.images.is_none(),
                "unchanged images cloned in patch: {p:?}"
            );
            assert!(
                p.volumes.is_none(),
                "unchanged volumes cloned in patch: {p:?}"
            );
            assert!(
                p.networks.is_none(),
                "unchanged networks cloned in patch: {p:?}"
            );
            assert!(p.usage.is_none(), "unchanged usage sent in patch: {p:?}");
        }
    }

    #[tokio::test]
    async fn noisy_container_events_are_ignored_while_lifecycle_events_refresh() {
        // P-007: exec_*, resize, top, attach do not trigger containers/json re-lists.
        let backend = Arc::new(FakeBackend::default());
        let (ev_tx, ev_rx) = futures::channel::mpsc::unbounded();
        *backend.events_rx.lock().unwrap() = Some(ev_rx);
        let driver = FakeDriver::new(backend.clone());
        connect_with(&driver, None);
        for _ in 0..20 {
            if backend.container_calls.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
        let baseline = backend.container_calls.load(Ordering::SeqCst);

        for noisy in [
            "exec_create: /bin/sh",
            "exec_start: /bin/sh",
            "exec_die",
            "resize",
            "top",
            "attach",
        ] {
            ev_tx
                .unbounded_send(Ok(engine::EngineEvent {
                    kind: ResourceKind::Container,
                    action: noisy.into(),
                    id: "c1".into(),
                    name: None,
                    exit_code: None,
                }))
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(backend.container_calls.load(Ordering::SeqCst), baseline);

        // Positive control: health_status: healthy and die DO refresh containers.
        ev_tx
            .unbounded_send(Ok(engine::EngineEvent {
                kind: ResourceKind::Container,
                action: "health_status: healthy".into(),
                id: "c1".into(),
                name: None,
                exit_code: None,
            }))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(backend.container_calls.load(Ordering::SeqCst), baseline + 1);
    }

    #[tokio::test]
    async fn a_crash_on_the_event_stream_is_reported_and_a_stop_is_not() {
        let backend = Arc::new(FakeBackend::default());
        let (ev_tx, ev_rx) = futures::channel::mpsc::unbounded();
        *backend.events_rx.lock().unwrap() = Some(ev_rx);
        let driver = FakeDriver::new(backend.clone());
        connect_with(&driver, None);
        for _ in 0..20 {
            if backend.container_calls.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let send = |action: &str, id: &str, code: Option<i64>| {
            ev_tx
                .unbounded_send(Ok(engine::EngineEvent {
                    kind: ResourceKind::Container,
                    action: action.into(),
                    id: id.into(),
                    name: Some(format!("{id}-name")),
                    exit_code: code,
                }))
                .unwrap();
        };
        send("kill", "stopped", None);
        send("die", "stopped", Some(143));
        send("die", "crashed", Some(2));
        tokio::time::sleep(Duration::from_millis(25)).await;
        let exits = driver.0.exits.lock().unwrap().clone();
        assert_eq!(
            exits,
            [dto::ContainerExit {
                id: "crashed".into(),
                name: "crashed-name".into(),
                cause: dto::ExitCause::Code(2),
            }]
        );
    }

    #[tokio::test]
    async fn compose_ring_caps_lines_evicts_finished_runs_and_supports_cancel_and_dismiss() {
        // P-009 & P-010: VecDeque 500-line ring, MAX_COMPOSE_RUNS=8 eviction, cancel & dismiss.
        let backend = Arc::new(FakeBackend::default());
        *backend.compose_factory.lock().unwrap() = Some(Box::new(|| {
            let lines = (0..600)
                .map(|i| ComposeOutput::Line(format!("line-{i}")))
                .chain(std::iter::once(ComposeOutput::Exit(Ok(()))));
            Box::pin(futures::stream::iter(lines))
        }));
        let driver = FakeDriver::new(backend.clone());
        connect_with(&driver, None);
        tokio::time::sleep(Duration::from_millis(15)).await;

        for p in 0..10 {
            compose_with(
                &driver,
                engine::ComposeLabels {
                    project: format!("proj-{p:02}"),
                    service: "app".into(),
                    working_dir: None,
                    config_files: Vec::new(),
                },
                engine::ComposeCommand::Up,
            )
            .unwrap();
            for _ in 0..20 {
                if !driver
                    .state()
                    .snapshot()
                    .pending
                    .contains(&format!("compose:proj-{p:02}"))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        let snap = driver.state().snapshot();
        assert_eq!(snap.compose_runs.len(), MAX_COMPOSE_RUNS);
        assert!(!snap.compose_runs.iter().any(|r| r.project == "proj-00"));
        assert!(!snap.compose_runs.iter().any(|r| r.project == "proj-01"));
        let latest = snap
            .compose_runs
            .iter()
            .find(|r| r.project == "proj-09")
            .unwrap();
        assert_eq!(latest.lines.len(), COMPOSE_LINES);
        assert_eq!(latest.lines.first().map(String::as_str), Some("line-100"));
        assert_eq!(latest.lines.last().map(String::as_str), Some("line-599"));

        // Dismiss removes a finished run.
        dismiss_compose_with(&driver, "proj-09");
        assert!(
            !driver
                .state()
                .snapshot()
                .compose_runs
                .iter()
                .any(|r| r.project == "proj-09")
        );

        // Cancel aborts an active compose stream immediately.
        let cancel_token = Arc::new(());
        let held = cancel_token.clone();
        *backend.compose_factory.lock().unwrap() = Some(Box::new(move || {
            let keep = held.clone();
            Box::pin(futures::stream::poll_fn(move |_| {
                let _ = &keep;
                std::task::Poll::Pending
            }))
        }));
        compose_with(
            &driver,
            engine::ComposeLabels {
                project: "stuck".into(),
                service: "app".into(),
                working_dir: None,
                config_files: Vec::new(),
            },
            engine::ComposeCommand::Up,
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        *backend.compose_factory.lock().unwrap() = None;
        assert_eq!(Arc::strong_count(&cancel_token), 2);

        cancel_compose_with(&driver, "stuck").unwrap();
        for _ in 0..20 {
            if Arc::strong_count(&cancel_token) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(Arc::strong_count(&cancel_token), 1);
        let stuck_run = driver
            .state()
            .snapshot()
            .compose_runs
            .into_iter()
            .find(|r| r.project == "stuck")
            .unwrap();
        assert_eq!(
            stuck_run.result,
            RunResult::Failed {
                message: "Cancelled.".into()
            }
        );
    }

    #[tokio::test]
    async fn superseding_connect_aborts_prior_inflight_connect_task() {
        // P-012: retry_now / context switch aborts any stuck prior connect task.
        let backend = Arc::new(FakeBackend::default());
        let stuck_token = Arc::new(());
        let held = stuck_token.clone();
        let backend_cl = backend.clone();
        let mut call = 0usize;
        *backend.connect_factory.lock().unwrap() = Some(Box::new(move |target| {
            call += 1;
            if call == 1 {
                let keep = held.clone();
                Box::pin(futures::future::poll_fn(move |_| {
                    let _ = &keep;
                    std::task::Poll::Pending
                }))
            } else {
                let b = backend_cl.clone();
                Box::pin(async move {
                    Ok((
                        FakeClient(b),
                        EngineInfo {
                            version: "27.0".into(),
                            api_version: "1.46".into(),
                            os: "linux".into(),
                            arch: "amd64".into(),
                            host: target.host,
                        },
                    ))
                })
            }
        }));

        let driver = FakeDriver::new(backend.clone());
        connect_with(&driver, None);
        tokio::time::sleep(Duration::from_millis(10)).await;
        // held inside closure + held inside active connect future = 3; drop factory to isolate active task!
        *backend.connect_factory.lock().unwrap() = None;
        assert_eq!(Arc::strong_count(&stuck_token), 2);

        // Supersede with a new connect: prior connect future must be aborted immediately.
        connect_with(&driver, Some(test_target("unix:///fake2")));
        for _ in 0..20 {
            if Arc::strong_count(&stuck_token) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(Arc::strong_count(&stuck_token), 1);
    }

    #[tokio::test]
    async fn usage_polling_computes_cpu_delta_and_resets_when_containers_stop() {
        let backend = Arc::new(FakeBackend::default());
        *backend.capacity.lock().unwrap() = engine::SystemCapacity {
            cpu_cores: 8,
            mem_total: 16 * 1024 * 1024 * 1024,
            disk_total: Some(500_000_000_000),
        };
        *backend.containers.lock().unwrap() = vec![engine::ContainerSummary {
            id: "c1".into(),
            name: "web".into(),
            image: "nginx:1.27".into(),
            state: engine::ContainerState::Running,
            status: "Up 2 minutes".into(),
            created: 1,
            ports: Vec::new(),
            compose: None,
            networks: Vec::new(),
            volumes: Vec::new(),
        }];
        backend.usage_samples.lock().unwrap().extend([
            engine::ContainerUsageSample {
                mem_usage: 512 * 1024 * 1024,
                cpu_readings: HashMap::from([(
                    "c1".into(),
                    engine::ContainerCpuReading {
                        total_usage: 1_000_000_000,
                        system_usage: 10_000_000_000,
                        online_cpus: Some(8),
                    },
                )]),
            },
            engine::ContainerUsageSample {
                mem_usage: 768 * 1024 * 1024,
                cpu_readings: HashMap::from([(
                    "c1".into(),
                    engine::ContainerCpuReading {
                        total_usage: 1_500_000_000,
                        system_usage: 18_000_000_000,
                        online_cpus: Some(8),
                    },
                )]),
            },
        ]);

        let driver = FakeDriver::new(backend.clone());
        driver.0.first_cpu_ms.store(10, Ordering::SeqCst);
        driver.0.usage_poll_ms.store(500, Ordering::SeqCst);
        connect_with(&driver, None);

        for _ in 0..40 {
            if driver.state().snapshot().usage.cpu_used_cores.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let usage = driver.state().snapshot().usage;
        assert_eq!(usage.cpu_cores, Some(8));
        assert_eq!(usage.mem_total, Some((16u64 * 1024 * 1024 * 1024) as f64));
        assert_eq!(usage.disk_total, Some(500_000_000_000.0));
        assert_eq!(usage.mem_usage, Some((768u64 * 1024 * 1024) as f64));
        let cores = usage
            .cpu_used_cores
            .expect("second sample computes CPU delta");
        assert!(
            (cores - 0.5).abs() < 1e-9,
            "expected 0.5 cores, got {cores}"
        );

        // Wait for emit_duration to settle, clear recorded patches, then trigger a usage-only update
        // by stopping all containers via container refresh -> usage_notify wakes usage_task immediately.
        tokio::time::sleep(Duration::from_millis(15)).await;
        driver.0.patches.lock().unwrap().clear();

        backend.containers.lock().unwrap()[0].state = engine::ContainerState::Exited;
        refresh_with(&driver, ResourceKind::Container);

        for _ in 0..30 {
            if driver.state().snapshot().usage.cpu_used_cores == Some(0.0) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let stopped = driver.state().snapshot().usage;
        assert_eq!(stopped.cpu_used_cores, Some(0.0));
        assert_eq!(stopped.mem_usage, Some(0.0));
        assert!(
            driver
                .0
                .patches
                .lock()
                .unwrap()
                .iter()
                .all(|p| p.images.is_none() && p.volumes.is_none() && p.networks.is_none()),
            "container/usage updates must not clone unchanged slices"
        );
    }

    #[tokio::test]
    async fn disk_usage_refreshes_on_storage_events_persists_while_reconnecting_and_clears_on_target_switch()
     {
        let backend = Arc::new(FakeBackend::default());
        *backend.disk.lock().unwrap() = Some(engine::DiskUsage {
            images: 1_000,
            containers: 200,
            volumes: 300,
            build_cache: 500,
            ..Default::default()
        });
        let (ev_tx, ev_rx) = futures::channel::mpsc::unbounded();
        *backend.events_rx.lock().unwrap() = Some(ev_rx);

        let driver = FakeDriver::new(backend.clone());
        connect_with(&driver, None);

        for _ in 0..30 {
            if driver.state().snapshot().usage.disk_usage == Some(2_000.0) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let initial = driver.state().snapshot().usage;
        assert_eq!(initial.disk_usage, Some(2_000.0));
        assert_eq!(
            initial.disk_breakdown,
            Some(dto::DiskBreakdown {
                images: 1_000.0,
                containers: 200.0,
                volumes: 300.0,
                build_cache: 500.0,
            })
        );

        *backend.disk.lock().unwrap() = Some(engine::DiskUsage {
            images: 3_000,
            containers: 200,
            volumes: 300,
            build_cache: 500,
            ..Default::default()
        });
        ev_tx
            .unbounded_send(Ok(engine::EngineEvent {
                kind: ResourceKind::Image,
                action: "pull".into(),
                id: "sha256:abc".into(),
                name: None,
                exit_code: None,
            }))
            .unwrap();

        for _ in 0..30 {
            if driver.state().snapshot().usage.disk_usage == Some(4_000.0) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(driver.state().snapshot().usage.disk_usage, Some(4_000.0));

        // Connection loss keeps last-known usage while Reconnecting, then target switch clears it.
        *backend.connect_factory.lock().unwrap() =
            Some(Box::new(|_| Box::pin(futures::future::pending())));
        ev_tx
            .unbounded_send(Err(EngineError::Transport("eof".into())))
            .unwrap();
        for _ in 0..20 {
            if matches!(
                driver.state().snapshot().conn,
                ConnState::Reconnecting { .. }
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(driver.state().snapshot().usage.disk_usage, Some(4_000.0));

        connect_with(&driver, Some(test_target("tcp://remote:2375")));
        let switched = driver.state().snapshot();
        assert!(matches!(switched.conn, ConnState::Connecting));
        assert_eq!(switched.usage, dto::SystemUsage::default());
    }

    /// A pull whose stream yields `items` and then ends.
    fn pull_yields(backend: &FakeBackend, items: Vec<Result<PullProgress, EngineError>>) {
        let items = Mutex::new(Some(items));
        *backend.pull_factory.lock().unwrap() = Some(Box::new(move || {
            Box::pin(futures::stream::iter(
                items.lock().unwrap().take().unwrap_or_default(),
            ))
        }));
    }

    #[tokio::test]
    async fn a_second_pull_of_the_same_reference_is_refused_while_the_first_runs() {
        let backend = Arc::new(FakeBackend::default());
        *backend.pull_factory.lock().unwrap() =
            Some(Box::new(|| Box::pin(futures::stream::pending())));
        let driver = connected(backend).await;
        let client = driver.state().engine().unwrap();

        pull_with(&driver, client.clone(), "nginx:1.27".into()).unwrap();
        let again = pull_with(&driver, client.clone(), "nginx:1.27".into());
        assert_eq!(again.unwrap_err().kind, dto::UiErrorKind::Conflict);
        // Another reference is not blocked.
        pull_with(&driver, client, "alpine:3.20".into()).unwrap();
        let pulls = driver.state().snapshot().pulls;
        assert_eq!(pulls.len(), 2, "{pulls:?}");
    }

    #[tokio::test]
    async fn a_finished_pull_says_so_and_re_lists_images_and_disk_usage() {
        let backend = Arc::new(FakeBackend::default());
        pull_yields(
            &backend,
            vec![Ok(PullProgress {
                layer: Some("l1".into()),
                status: "Pull complete".into(),
                current: None,
                total: None,
            })],
        );
        let driver = connected(backend.clone()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let (images, disk) = (
            backend.image_calls.load(Ordering::SeqCst),
            backend.disk_calls.load(Ordering::SeqCst),
        );

        let client = driver.state().engine().unwrap();
        pull_with(&driver, client, "nginx:1.27".into()).unwrap();
        settle_until(|| backend.image_calls.load(Ordering::SeqCst) > images).await;
        settle_until(|| backend.disk_calls.load(Ordering::SeqCst) > disk).await;
        let notices = driver.0.notices.lock().unwrap().clone();
        assert_eq!(notices, [Notice::Pulled("nginx:1.27".into())]);
        assert!(driver.state().snapshot().pulls.is_empty());
        assert!(driver.state().lock().pull_tasks.is_empty());
    }

    #[tokio::test]
    async fn a_failed_pull_says_why_and_can_be_tried_again() {
        let backend = Arc::new(FakeBackend::default());
        pull_yields(
            &backend,
            vec![Err(EngineError::NotFound("manifest unknown".into()))],
        );
        let driver = connected(backend.clone()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let images = backend.image_calls.load(Ordering::SeqCst);

        let client = driver.state().engine().unwrap();
        pull_with(&driver, client.clone(), "nginx:nope".into()).unwrap();
        settle_until(|| !driver.0.notices.lock().unwrap().is_empty()).await;
        let notices = driver.0.notices.lock().unwrap().clone();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            matches!(&notices[0], Notice::PullFailed { reference, .. } if reference == "nginx:nope"),
            "{notices:?}"
        );
        assert!(driver.state().snapshot().pulls.is_empty());
        assert!(driver.state().lock().pull_tasks.is_empty());
        // Nothing was pulled, so images are not re-listed, and the connection stays up.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(backend.image_calls.load(Ordering::SeqCst), images);
        assert!(matches!(
            driver.state().snapshot().conn,
            ConnState::Connected { .. }
        ));
        // The reference is free again.
        pull_with(&driver, client, "nginx:nope".into()).unwrap();
    }

    #[tokio::test]
    async fn a_pull_that_loses_the_connection_reconnects_instead_of_a_notice() {
        let backend = Arc::new(FakeBackend::default());
        pull_yields(&backend, vec![Err(EngineError::Transport("eof".into()))]);
        let driver = connected(backend.clone()).await;
        *backend.connect_factory.lock().unwrap() =
            Some(Box::new(|_| Box::pin(futures::future::pending())));

        let client = driver.state().engine().unwrap();
        pull_with(&driver, client, "nginx:1.27".into()).unwrap();
        settle_until(|| {
            matches!(
                driver.state().snapshot().conn,
                ConnState::Reconnecting { .. }
            )
        })
        .await;
        assert!(driver.0.notices.lock().unwrap().is_empty());
        assert!(driver.state().snapshot().pulls.is_empty());
    }

    /// Connects with one running container, then makes `call` fail with `error` and
    /// re-reads everything (capacity, every list, which wakes the usage poll, and disk).
    async fn fail_in_the_background(call: &'static str, error: EngineError) -> FakeDriver {
        let backend = Arc::new(FakeBackend::default());
        *backend.containers.lock().unwrap() = vec![engine::ContainerSummary {
            id: "c1".into(),
            name: "web".into(),
            image: "nginx:1.27".into(),
            state: engine::ContainerState::Running,
            status: "Up 2 minutes".into(),
            created: 1,
            ports: Vec::new(),
            compose: None,
            networks: Vec::new(),
            volumes: Vec::new(),
        }];
        let driver = connected(backend.clone()).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        *backend.connect_factory.lock().unwrap() =
            Some(Box::new(|_| Box::pin(futures::future::pending())));
        backend.failures.lock().unwrap().insert(call, error);
        refresh_all_with(&driver);
        driver
    }

    const BACKGROUND_CALLS: [&str; 4] = ["capacity", "images", "usage", "disk"];

    #[tokio::test]
    async fn a_background_read_that_loses_the_connection_reconnects_without_a_notice() {
        for call in BACKGROUND_CALLS {
            let driver = fail_in_the_background(call, EngineError::Transport("eof".into())).await;
            settle_until(|| {
                matches!(
                    driver.state().snapshot().conn,
                    ConnState::Reconnecting { .. }
                )
            })
            .await;
            assert!(driver.0.notices.lock().unwrap().is_empty(), "{call}");
        }
    }

    #[tokio::test]
    async fn losing_the_connection_on_the_first_capacity_read_reconnects() {
        let backend = Arc::new(FakeBackend::default());
        backend
            .failures
            .lock()
            .unwrap()
            .insert("capacity", EngineError::Transport("eof".into()));
        // The first connect succeeds; the retry it schedules then waits forever.
        let first = Mutex::new(Some(FakeClient(backend.clone())));
        *backend.connect_factory.lock().unwrap() =
            Some(Box::new(move |target| match first.lock().unwrap().take() {
                Some(client) => Box::pin(async move {
                    Ok((
                        client,
                        EngineInfo {
                            version: "27.0".into(),
                            api_version: "1.46".into(),
                            os: "linux".into(),
                            arch: "amd64".into(),
                            host: target.host,
                        },
                    ))
                }),
                None => Box::pin(futures::future::pending()),
            }));
        let driver = FakeDriver::new(backend);
        connect_with(&driver, None);
        settle_until(|| {
            matches!(
                driver.state().snapshot().conn,
                ConnState::Reconnecting { .. }
            )
        })
        .await;
        assert!(driver.0.notices.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_background_read_that_fails_keeps_the_connection_and_only_a_list_says_so() {
        for call in BACKGROUND_CALLS {
            let error = EngineError::Daemon {
                status: 500,
                message: "busy".into(),
            };
            let driver = fail_in_the_background(call, error).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(
                matches!(driver.state().snapshot().conn, ConnState::Connected { .. }),
                "{call}"
            );
            let notices = driver.0.notices.lock().unwrap().clone();
            if call == "images" {
                // A list the user is looking at did not load: say so, and keep the old one.
                assert_eq!(notices.len(), 1, "{notices:?}");
                assert_eq!(notices[0].kind(), NoticeKind::Error);
            } else {
                // Meters are polled again; a failed sample is not worth a toast.
                assert!(notices.is_empty(), "{call}: {notices:?}");
            }
        }
    }

    fn labels(project: &str) -> engine::ComposeLabels {
        engine::ComposeLabels {
            project: project.into(),
            service: "app".into(),
            working_dir: None,
            config_files: Vec::new(),
        }
    }

    fn compose_run(driver: &FakeDriver, project: &str) -> Option<dto::ComposeRun> {
        driver
            .state()
            .snapshot()
            .compose_runs
            .into_iter()
            .find(|r| r.project == project)
    }

    #[tokio::test]
    async fn a_second_compose_command_for_a_project_is_refused_while_one_runs() {
        let backend = Arc::new(FakeBackend::default());
        *backend.compose_factory.lock().unwrap() =
            Some(Box::new(|| Box::pin(futures::stream::pending())));
        let driver = connected(backend).await;

        compose_with(&driver, labels("shop"), engine::ComposeCommand::Up).unwrap();
        let again = compose_with(&driver, labels("shop"), engine::ComposeCommand::Down);
        assert_eq!(again.unwrap_err().kind, dto::UiErrorKind::Conflict);
        assert_eq!(
            compose_run(&driver, "shop").unwrap().command,
            "docker compose up -d"
        );
        // Another project is not blocked.
        compose_with(&driver, labels("blog"), engine::ComposeCommand::Up).unwrap();
    }

    #[tokio::test]
    async fn a_failed_compose_command_says_why_and_frees_its_project() {
        let backend = Arc::new(FakeBackend::default());
        *backend.compose_factory.lock().unwrap() = Some(Box::new(|| {
            Box::pin(futures::stream::iter([
                ComposeOutput::Line("pulling".into()),
                ComposeOutput::Exit(Err("no such service: web".into())),
            ]))
        }));
        let driver = connected(backend).await;

        compose_with(&driver, labels("shop"), engine::ComposeCommand::Up).unwrap();
        settle_until(|| !driver.0.notices.lock().unwrap().is_empty()).await;
        let notices = driver.0.notices.lock().unwrap().clone();
        assert_eq!(notices, [Notice::Error("no such service: web".into())]);
        let run = compose_run(&driver, "shop").unwrap();
        assert_eq!(run.lines, ["pulling"]);
        assert_eq!(
            run.result,
            RunResult::Failed {
                message: "no such service: web".into()
            }
        );
        assert!(!is_pending(&driver, "compose:shop"));
    }

    #[tokio::test]
    async fn a_compose_command_that_ends_without_an_exit_is_marked_failed() {
        let backend = Arc::new(FakeBackend::default());
        *backend.compose_factory.lock().unwrap() = Some(Box::new(|| {
            Box::pin(futures::stream::iter([ComposeOutput::Line("x".into())]))
        }));
        let driver = connected(backend).await;

        compose_with(&driver, labels("shop"), engine::ComposeCommand::Up).unwrap();
        settle_until(|| !is_pending(&driver, "compose:shop")).await;
        assert_eq!(
            compose_run(&driver, "shop").unwrap().result,
            RunResult::Failed {
                message: "Compose command ended unexpectedly.".into()
            }
        );
        assert!(driver.0.notices.lock().unwrap().is_empty());
        // The project is free again.
        compose_with(&driver, labels("shop"), engine::ComposeCommand::Up).unwrap();
    }
}
