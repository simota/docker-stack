//! What the screens show, computed from snapshots without touching widgets: list rows,
//! the connection banner, usage meters, log lines and redaction.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use crate::dto::{
    CleanupImages, CleanupReport, CleanupScope, CleanupVolumes, ConnState, Container,
    ContainerExit, ContainerState, ContainerUpdate, DaemonStatus, ExitCause, Freshness, LogStream,
    Network, Notice, NoticeKind, SkippedSetting, Snapshot, StatePatch, SystemUsage, Target,
    UiErrorKind, UpdateCheck, UpdateOutcome, Volume,
};

use super::i18n::{TimeUnit, Tr};

pub fn format_bytes(n: f64) -> String {
    engine::format_bytes(n.max(0.0) as u64)
}

pub fn short_id(id: &str) -> &str {
    let id = id.strip_prefix("sha256:").unwrap_or(id);
    &id[..id.len().min(12)]
}

pub fn relative_time(unix_secs: f64, now_secs: f64, t: Tr) -> String {
    let d = (now_secs - unix_secs).max(0.0) as u64;
    const STEPS: [(u64, TimeUnit); 5] = [
        (31_536_000, TimeUnit::Year),
        (2_592_000, TimeUnit::Month),
        (86_400, TimeUnit::Day),
        (3_600, TimeUnit::Hour),
        (60, TimeUnit::Minute),
    ];
    STEPS.iter().find(|(secs, _)| d >= *secs).map_or_else(
        || t.just_now().to_owned(),
        |&(secs, unit)| t.ago(d / secs, unit),
    )
}

pub fn is_running(c: &Container) -> bool {
    matches!(
        c.state,
        ContainerState::Running | ContainerState::Restarting
    )
}

/// Whether Start can run on `c`: created or exited (a paused one is resumed instead,
/// and dockerd refuses to start a dead or removing one).
pub fn startable(c: &Container) -> bool {
    matches!(c.state, ContainerState::Created | ContainerState::Exited)
}

/// Whether Stop can run on `c` (a paused container stops too).
pub fn stoppable(c: &Container) -> bool {
    is_running(c) || c.state == ContainerState::Paused
}

/// A cell of the deck's emblem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Running,
    Paused,
    Idle,
}

/// The emblem's six cells: running containers' share in the first ones (at least one
/// when any runs), then paused or restarting ones, the rest idle.
pub fn emblem_slots(containers: &[Container]) -> [Slot; 6] {
    let total = containers.len();
    let mut slots = [Slot::Idle; 6];
    if total == 0 {
        return slots;
    }
    let share = |n: usize| ((n as f64 / total as f64) * 6.0).round() as usize;
    let running = containers
        .iter()
        .filter(|c| c.state == ContainerState::Running)
        .count();
    let paused = containers
        .iter()
        .filter(|c| matches!(c.state, ContainerState::Paused | ContainerState::Restarting))
        .count();
    let lit = share(running).max(usize::from(running > 0)).min(6);
    let warn = if paused > 0 {
        share(paused).max(1).min(6 - lit)
    } else {
        0
    };
    for (i, slot) in slots.iter_mut().enumerate() {
        if i < lit {
            *slot = Slot::Running;
        } else if i < lit + warn {
            *slot = Slot::Paused;
        }
    }
    slots
}

pub fn state_label(s: ContainerState) -> &'static str {
    match s {
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

/// Applies a delta patch; stale revisions are ignored.
pub fn apply_patch(s: &mut Snapshot, p: StatePatch) {
    if p.revision <= s.revision {
        return;
    }
    s.revision = p.revision;
    s.conn = p.conn;
    s.target = p.target;
    s.daemon = p.daemon;
    s.compose_available = p.compose_available;
    if let Some(v) = p.containers {
        s.containers = v;
    }
    if let Some(v) = p.images {
        s.images = v;
    }
    if let Some(v) = p.volumes {
        s.volumes = v;
    }
    if let Some(v) = p.networks {
        s.networks = v;
    }
    if let Some(v) = p.pending {
        s.pending = v;
    }
    if let Some(v) = p.pulls {
        s.pulls = v;
    }
    if let Some(v) = p.compose_runs {
        s.compose_runs = v;
    }
    if let Some(v) = p.usage {
        s.usage = v;
    }
}

// ---- container rows ----

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Group {
        project: String,
        running: usize,
        total: usize,
        collapsed: bool,
    },
    Container {
        id: String,
        nested: bool,
    },
}

impl Row {
    /// Stable identity, used to keep the selection across updates.
    pub fn key(&self) -> String {
        match self {
            Row::Group { project, .. } => format!("group:{project}"),
            Row::Container { id, .. } => id.clone(),
        }
    }
}

fn matches(c: &Container, needle: &str) -> bool {
    needle.is_empty()
        || c.name.to_lowercase().contains(needle)
        || c.image.to_lowercase().contains(needle)
        || c.id.starts_with(needle)
        || c.compose
            .as_ref()
            .is_some_and(|l| l.project.to_lowercase().contains(needle))
}

/// Standalone containers come first, then the stacks, each sorted by name (the table
/// takes every row after a stack's row, up to the next one, as its member). A group shows
/// when any member passes the filters; its counts cover the whole project.
pub fn build_rows(
    containers: &[Container],
    filter: &str,
    show_stopped: bool,
    collapsed: &BTreeSet<String>,
) -> Vec<Row> {
    let needle = filter.trim().to_lowercase();
    let visible = |c: &Container| (show_stopped || is_running(c)) && matches(c, &needle);
    let mut groups: BTreeMap<&str, Vec<&Container>> = BTreeMap::new();
    // (is a stack, name, rows)
    let mut entries: Vec<(bool, String, Vec<Row>)> = Vec::new();
    for c in containers {
        if let Some(l) = &c.compose {
            groups.entry(&l.project).or_default().push(c);
        } else if visible(c) {
            entries.push((
                false,
                c.name.to_lowercase(),
                vec![Row::Container {
                    id: c.id.clone(),
                    nested: false,
                }],
            ));
        }
    }
    for (project, members) in &groups {
        let mut shown: Vec<&Container> = members.iter().copied().filter(|c| visible(c)).collect();
        if shown.is_empty() {
            continue;
        }
        let is_collapsed = collapsed.contains(*project);
        let mut rows = vec![Row::Group {
            project: (*project).to_owned(),
            running: members.iter().filter(|c| is_running(c)).count(),
            total: members.len(),
            collapsed: is_collapsed,
        }];
        if !is_collapsed {
            shown.sort_by(|a, b| {
                fn service(c: &Container) -> Option<&str> {
                    c.compose.as_ref().map(|l| l.service.as_str())
                }
                service(a)
                    .cmp(&service(b))
                    .then_with(|| a.name.cmp(&b.name))
            });
            rows.extend(shown.into_iter().map(|c| Row::Container {
                id: c.id.clone(),
                nested: true,
            }));
        }
        entries.push((true, project.to_lowercase(), rows));
    }
    entries.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    entries.into_iter().flat_map(|(_, _, rows)| rows).collect()
}

/// How many containers (running or stopped) reference each name returned by `refs`.
pub fn usage_counts<'a>(
    containers: &'a [Container],
    refs: impl Fn(&'a Container) -> &'a [String],
) -> HashMap<&'a str, usize> {
    let mut counts = HashMap::new();
    // Each name counts once per container; the buffer is reused across containers.
    let mut unique: Vec<&str> = Vec::new();
    for c in containers {
        unique.clear();
        unique.extend(refs(c).iter().map(String::as_str));
        unique.sort_unstable();
        unique.dedup();
        for name in &unique {
            *counts.entry(*name).or_insert(0) += 1;
        }
    }
    counts
}

/// The tray menu's summary: how many containers run, and each stack with `running/total`,
/// sorted by name.
pub fn tray_summary(containers: &[Container]) -> (usize, Vec<(String, String)>) {
    let running = containers.iter().filter(|c| is_running(c)).count();
    let mut counts: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for c in containers {
        if let Some(l) = &c.compose {
            let e = counts.entry(l.project.as_str()).or_default();
            e.1 += 1;
            if is_running(c) {
                e.0 += 1;
            }
        }
    }
    let stacks = counts
        .into_iter()
        .map(|(p, (r, t))| (p.to_owned(), format!("{r}/{t}")))
        .collect();
    (running, stacks)
}

/// API 1.42 (Docker 23) made `volume prune` remove only anonymous volumes by default.
pub fn prune_removes_named_volumes(api_version: Option<&str>) -> bool {
    engine::prune_removes_named_volumes(api_version)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupToggle {
    Containers,
    Networks,
    DanglingImages,
    TaggedImages,
    Volumes,
    NamedVolumes,
    BuildCache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupForm {
    pub containers: bool,
    pub networks: bool,
    pub dangling_images: bool,
    pub tagged_images: bool,
    pub volumes: bool,
    pub named_volumes: bool,
    pub build_cache: bool,
    /// True on API < 1.42, where volume prune always deletes named volumes too.
    pub legacy_volumes: bool,
}

impl CleanupForm {
    pub fn new(api_version: Option<&str>) -> Self {
        let legacy_volumes = prune_removes_named_volumes(api_version);
        Self {
            containers: true,
            networks: true,
            dangling_images: true,
            tagged_images: false,
            volumes: !legacy_volumes,
            named_volumes: false,
            build_cache: true,
            legacy_volumes,
        }
    }

    pub fn sync_api(&mut self, api_version: Option<&str>) {
        let legacy = prune_removes_named_volumes(api_version);
        if legacy && !self.legacy_volumes {
            // Never carry an anonymous-only default across to a legacy daemon where it
            // would delete named volumes without consent.
            self.volumes = false;
            self.named_volumes = false;
        }
        self.legacy_volumes = legacy;
    }

    pub fn set(&mut self, toggle: CleanupToggle, on: bool) {
        match toggle {
            CleanupToggle::Containers => self.containers = on,
            CleanupToggle::Networks => self.networks = on,
            CleanupToggle::DanglingImages => {
                self.dangling_images = on;
                if !on {
                    self.tagged_images = false;
                }
            }
            CleanupToggle::TaggedImages => {
                self.tagged_images = on;
                if on {
                    self.dangling_images = true;
                }
            }
            CleanupToggle::Volumes => {
                self.volumes = on;
                if !on {
                    self.named_volumes = false;
                }
            }
            CleanupToggle::NamedVolumes => {
                if !self.legacy_volumes {
                    self.named_volumes = on;
                    if on {
                        self.volumes = true;
                    }
                }
            }
            CleanupToggle::BuildCache => self.build_cache = on,
        }
    }

    pub fn scope(&self) -> CleanupScope {
        let images = if !self.dangling_images {
            None
        } else if self.tagged_images {
            Some(CleanupImages::AllUnused)
        } else {
            Some(CleanupImages::Dangling)
        };
        let volumes = if !self.volumes {
            None
        } else if self.legacy_volumes || self.named_volumes {
            Some(CleanupVolumes::AllUnused)
        } else {
            Some(CleanupVolumes::Anonymous)
        };
        CleanupScope {
            containers: self.containers,
            networks: self.networks,
            images,
            volumes,
            build_cache: self.build_cache,
        }
    }

    pub fn warns_named_volumes(&self) -> bool {
        self.volumes && (self.legacy_volumes || self.named_volumes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupPreview {
    pub containers: String,
    pub networks: String,
    pub dangling_images: String,
    pub tagged_images: String,
    pub volumes: String,
    pub named_volumes: String,
    pub build_cache: String,
    pub warn_named_volumes: bool,
    pub prune_pending: bool,
    pub can_submit: bool,
}

fn format_volume_group(volumes: &[&Volume], t: Tr) -> String {
    let n = volumes.len();
    if n == 0 {
        return t.cleanup_count(0);
    }
    let mut known = false;
    let mut sum = 0.0;
    for v in volumes {
        if let Some(s) = v.size {
            known = true;
            sum += s;
        }
    }
    if known {
        t.cleanup_count_size(n, &format_bytes(sum))
    } else {
        t.cleanup_count(n)
    }
}

pub fn snapshot_api_version(snap: &Snapshot) -> Option<&str> {
    match &snap.conn {
        ConnState::Connected { info } => Some(info.api_version.as_str()),
        _ => None,
    }
}

pub fn cleanup_preview(snap: &Snapshot, form: &CleanupForm, t: Tr) -> CleanupPreview {
    let connected = matches!(snap.conn, ConnState::Connected { .. });
    let prune_pending = snap
        .pending
        .iter()
        .any(|k| k == crate::commands::PRUNE_PENDING_KEY);

    let stopped = snap
        .containers
        .iter()
        .filter(|c| !is_running(c) && c.state != ContainerState::Paused)
        .count();

    let kept_containers: Vec<Container> = if form.containers {
        snap.containers
            .iter()
            .filter(|c| is_running(c) || c.state == ContainerState::Paused)
            .cloned()
            .collect()
    } else {
        snap.containers.clone()
    };

    let used_nets = usage_counts(&kept_containers, |c| &c.networks);
    let unused_nets = snap
        .networks
        .iter()
        .filter(|n| !n.builtin && !used_nets.contains_key(n.name.as_str()))
        .count();

    let dangling: Vec<_> = snap
        .images
        .iter()
        .filter(|i| i.dangling && i.containers <= 0.0)
        .collect();
    let dangling_text = if dangling.is_empty() {
        t.cleanup_count(0)
    } else {
        let size: f64 = dangling.iter().map(|i| i.size).sum();
        t.cleanup_count_size(dangling.len(), &format_bytes(size))
    };

    let tagged_known = snap
        .images
        .iter()
        .any(|i| !i.dangling && i.containers >= 0.0);
    let tagged: Vec<_> = snap
        .images
        .iter()
        .filter(|i| !i.dangling && i.containers == 0.0)
        .collect();
    let tagged_text = if !tagged_known && snap.images.iter().any(|i| !i.dangling) {
        "—".to_owned()
    } else if tagged.is_empty() {
        t.cleanup_count(0)
    } else {
        let size: f64 = tagged.iter().map(|i| i.size).sum();
        t.cleanup_count_size(tagged.len(), &format_bytes(size))
    };

    let used_vols = usage_counts(&kept_containers, |c| &c.volumes);
    let unused_vols: Vec<&Volume> = snap
        .volumes
        .iter()
        .filter(|v| !used_vols.contains_key(v.name.as_str()))
        .collect();

    let (volumes_text, named_text) = if form.legacy_volumes {
        (format_volume_group(&unused_vols, t), String::new())
    } else {
        let anon: Vec<&Volume> = unused_vols
            .iter()
            .copied()
            .filter(|v| v.anonymous)
            .collect();
        let named: Vec<&Volume> = unused_vols
            .iter()
            .copied()
            .filter(|v| !v.anonymous)
            .collect();
        (
            format_volume_group(&anon, t),
            format_volume_group(&named, t),
        )
    };

    let build_cache = match snap.usage.disk_breakdown.map(|b| b.build_cache) {
        Some(bytes) if bytes > 0.0 => t.cleanup_up_to(&format_bytes(bytes)),
        Some(_) => format_bytes(0.0),
        None => "—".to_owned(),
    };

    CleanupPreview {
        containers: t.cleanup_count(stopped),
        networks: t.cleanup_count(unused_nets),
        dangling_images: dangling_text,
        tagged_images: tagged_text,
        volumes: volumes_text,
        named_volumes: named_text,
        build_cache,
        warn_named_volumes: form.warns_named_volumes(),
        prune_pending,
        can_submit: connected && !prune_pending && !form.scope().is_empty(),
    }
}

pub fn cleanup_notice(report: &CleanupReport, t: Tr) -> (NoticeKind, String) {
    let reclaimed = format_bytes(report.reclaimed);
    if let Some((step, err)) = &report.partial_error {
        (
            NoticeKind::Error,
            t.cleanup_partial(
                report.total_deleted(),
                &reclaimed,
                t.cleanup_step_name(*step),
                err,
            ),
        )
    } else {
        (
            NoticeKind::Success,
            t.cleanup_done(report.total_deleted(), &reclaimed),
        )
    }
}

// ---- connection ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConnView {
    pub tone: Tone,
    pub title: String,
    pub detail: String,
    /// Label of the retry button, when retrying makes sense.
    pub retry: Option<&'static str>,
    /// Label of the "start the local daemon" button, when the app can start it.
    pub start: Option<String>,
    /// Set while the app is starting the local daemon: (label, wait limit).
    pub starting: Option<(String, u32)>,
}

pub fn describe_conn(conn: &ConnState, target: &Target, daemon: &DaemonStatus, t: Tr) -> ConnView {
    let base = |tone, title, detail| ConnView {
        tone,
        title,
        detail,
        retry: None,
        start: None,
        starting: None,
    };
    if !matches!(conn, ConnState::Connected { .. })
        && let Some(label) = &daemon.starting
    {
        return ConnView {
            starting: Some((label.clone(), daemon.ready_timeout_secs)),
            ..base(Tone::Info, t.starting_daemon(label), target.host.clone())
        };
    }
    // Only offer to start the daemon when it looks down, not when the socket is merely slow.
    let start = match conn {
        ConnState::Unreachable { error, .. } | ConnState::Reconnecting { error, .. }
            if matches!(
                error.kind,
                UiErrorKind::SocketMissing | UiErrorKind::Transport
            ) =>
        {
            daemon.startable.as_deref().map(|l| t.start_daemon(l))
        }
        _ => None,
    };
    match conn {
        ConnState::Connecting => base(Tone::Info, t.connecting().into(), target.host.clone()),
        ConnState::Connected { info } => base(
            Tone::Ok,
            t.engine_title(&info.version),
            format!(
                "API {} · {}/{} · {}",
                info.api_version, info.os, info.arch, target.host
            ),
        ),
        ConnState::Reconnecting { error, retry_in } => ConnView {
            retry: Some(t.retry_now()),
            start,
            ..base(
                Tone::Warn,
                t.conn_lost().into(),
                t.retrying_in(&error.message, *retry_in),
            )
        },
        ConnState::Unreachable { error, retry_in } => ConnView {
            retry: Some(t.retry_now()),
            start,
            ..base(
                Tone::Error,
                t.unreachable().into(),
                t.retrying_in(&error.message, *retry_in),
            )
        },
        ConnState::PermissionDenied { .. } => ConnView {
            retry: Some(t.retry()),
            ..base(
                Tone::Error,
                t.permission_denied().into(),
                t.permission_detail().into(),
            )
        },
    }
}

// ---- usage meters ----

#[derive(Debug, Clone, PartialEq)]
pub struct Meter {
    pub label: &'static str,
    pub amount: String,
    pub percent: String,
    pub fraction: Option<f64>,
}

fn format_cores(cores: f64) -> String {
    if cores < 10.0 {
        format!("{cores:.2}")
    } else {
        format!("{cores:.1}")
    }
}

fn meter_rate(used: Option<f64>, total: Option<f64>) -> (String, Option<f64>) {
    match (used, total) {
        (Some(u), Some(t)) if t > 0.0 && u.is_finite() && t.is_finite() => {
            let ratio = (u / t).max(0.0);
            (format!("{:.1}%", ratio * 100.0), Some(ratio.min(1.0)))
        }
        _ => ("—".into(), None),
    }
}

fn byte_pair(used: Option<f64>, total: Option<f64>) -> String {
    let total = total.filter(|t| *t > 0.0);
    match (used, total) {
        (Some(u), Some(t)) => format!("{} / {}", format_bytes(u), format_bytes(t)),
        (None, Some(t)) => format!("— / {}", format_bytes(t)),
        (Some(u), None) => format_bytes(u),
        (None, None) => "—".into(),
    }
}

pub fn meters(u: &SystemUsage, t: Tr) -> [Meter; 3] {
    let cores = u.cpu_cores.filter(|c| *c > 0);
    let unit = t.cores(cores == Some(1));
    let cpu_amount = match (u.cpu_used_cores, cores) {
        (Some(used), Some(c)) => format!("{} / {c} {unit}", format_cores(used)),
        (None, Some(c)) => format!("— / {c} {unit}"),
        (Some(used), None) => format!("{} {}", format_cores(used), t.cores(false)),
        (None, None) => "—".into(),
    };
    let (cpu_pct, cpu_frac) = meter_rate(u.cpu_used_cores, cores.map(f64::from));
    let (mem_pct, mem_frac) = meter_rate(u.mem_usage, u.mem_total);
    let (disk_pct, disk_frac) = meter_rate(u.disk_usage, u.disk_total);
    [
        Meter {
            label: "CPU",
            amount: cpu_amount,
            percent: cpu_pct,
            fraction: cpu_frac,
        },
        Meter {
            label: "MEM",
            amount: byte_pair(u.mem_usage, u.mem_total),
            percent: mem_pct,
            fraction: mem_frac,
        },
        Meter {
            label: "DISK",
            amount: byte_pair(u.disk_usage, u.disk_total),
            percent: disk_pct,
            fraction: disk_frac,
        },
    ]
}

// ---- redaction ----

pub const REDACTED: &str = "<redacted>";

fn secret_name(name: &str) -> bool {
    let n = name.to_lowercase();
    const PARTS: [&str; 11] = [
        "pass",
        "secret",
        "token",
        "apikey",
        "api_key",
        "accesskey",
        "access_key",
        "privatekey",
        "private_key",
        "credential",
        "dsn",
    ];
    PARTS.iter().any(|p| n.contains(p))
        || ["auth", "cookie", "session"].iter().any(|p| n.contains(p))
        || n.ends_with("_key")
        || n == "key"
}

/// `scheme://user:password@host`
fn url_credentials(value: &str) -> bool {
    value.match_indices("://").any(|(i, _)| {
        let rest = &value[i + 3..];
        let seg_end = rest
            .find(|c: char| c == '/' || c.is_whitespace())
            .unwrap_or(rest.len());
        let seg = &rest[..seg_end];
        let Some(at) = seg.find('@') else {
            return false;
        };
        let userinfo = &seg[..at];
        userinfo
            .split_once(':')
            .is_some_and(|(user, pass)| !user.is_empty() && !pass.is_empty())
    })
}

/// The settings a duplicate leaves out, in the CLI's own terms (`bind /src → /app`,
/// `--entrypoint …`, `--network container:…`), or `None` when it keeps them all.
pub fn skipped_settings(skipped: &[SkippedSetting]) -> Option<String> {
    if skipped.is_empty() {
        return None;
    }
    let items: Vec<String> = skipped
        .iter()
        .map(|s| match s {
            SkippedSetting::Mount {
                kind,
                source: Some(source),
                target,
            } => format!("{kind} {source} → {target}"),
            SkippedSetting::Mount {
                kind,
                source: None,
                target,
            } => format!("{kind} {target}"),
            SkippedSetting::Entrypoint(e) if e.is_empty() => "--entrypoint \"\"".to_owned(),
            SkippedSetting::Entrypoint(e) => format!("--entrypoint {e}"),
            SkippedSetting::NetworkMode(mode) => format!("--network {mode}"),
            SkippedSetting::Env(key) => format!("-e {key}"),
            SkippedSetting::Option(flag) => flag.clone(),
        })
        .collect();
    Some(items.join(", "))
}

/// Whether Update can replace `c`: a compose project's containers are recreated by
/// the stack (its Pull and Up), which keeps the project's own settings.
pub fn updatable(c: &Container) -> bool {
    c.compose.is_none()
}

/// A change to a container's networks offered in its Networks dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkChoice {
    Connect(String),
    Disconnect(String),
}

/// What `c`'s networks can change to: connecting to each network it is not on, then
/// disconnecting from each it is on. A container on the host's network or none (or
/// sharing another container's) has no choices: those cannot be joined or left.
pub fn network_choices(c: &Container, networks: &[Network]) -> Vec<NetworkChoice> {
    let fixed = |name: &str| name == "host" || name == "none";
    if c.networks.is_empty() || c.networks.iter().any(|n| fixed(n)) {
        return Vec::new();
    }
    let mut connect: Vec<&str> = networks
        .iter()
        .map(|n| n.name.as_str())
        .filter(|n| !fixed(n) && !c.networks.iter().any(|on| on == n))
        .collect();
    connect.sort_unstable();
    connect.dedup();
    let mut disconnect: Vec<&str> = c.networks.iter().map(String::as_str).collect();
    disconnect.sort_unstable();
    connect
        .into_iter()
        .map(|n| NetworkChoice::Connect(n.to_owned()))
        .chain(
            disconnect
                .into_iter()
                .map(|n| NetworkChoice::Disconnect(n.to_owned())),
        )
        .collect()
}

/// The line under a container's status after a check: whether Update has a newer
/// image to move it to.
pub fn freshness_note(freshness: Option<&Freshness>, t: Tr) -> &'static str {
    match freshness {
        Some(Freshness::Available) => t.update_available(),
        Some(Freshness::Pulled) => t.update_pulled(),
        Some(Freshness::Current | Freshness::Unknown(_)) | None => "",
    }
}

/// The containers "Update all" replaces after a check: the ones with a newer image
/// that Update can replace (not a stack's), as (id, name), by name.
pub fn update_targets(
    containers: &[Container],
    updates: &HashMap<String, Freshness>,
) -> Vec<(String, String)> {
    let mut targets: Vec<(String, String)> = containers
        .iter()
        .filter(|c| updatable(c))
        .filter(|c| {
            matches!(
                updates.get(&c.id),
                Some(Freshness::Available | Freshness::Pulled)
            )
        })
        .map(|c| (c.id.clone(), c.name.clone()))
        .collect();
    targets.sort_by(|a, b| a.1.cmp(&b.1));
    targets
}

/// The toast after a check for newer images.
pub fn update_check_summary(checks: &[UpdateCheck], t: Tr) -> String {
    let count = |f: fn(&Freshness) -> bool| checks.iter().filter(|c| f(&c.freshness)).count();
    t.update_check_result(
        count(|f| matches!(f, Freshness::Available | Freshness::Pulled)),
        count(|f| matches!(f, Freshness::Unknown(_))),
    )
}

/// The toast for a finished Update.
pub fn update_notice(update: &ContainerUpdate, t: Tr) -> (NoticeKind, String) {
    match &update.outcome {
        UpdateOutcome::UpToDate => (NoticeKind::Success, t.update_up_to_date(&update.name)),
        UpdateOutcome::Updated => (NoticeKind::Success, t.updated(&update.name)),
        UpdateOutcome::UpdatedKeptOld(old) => {
            (NoticeKind::Success, t.updated_kept_old(&update.name, old))
        }
        UpdateOutcome::NotRestored {
            error,
            kept_as,
            stopped,
        } => (
            NoticeKind::Error,
            t.update_not_restored(&update.name, error, kept_as, *stopped),
        ),
    }
}

/// The toast for a [`Notice`] from the backend.
pub fn notice_text(notice: &Notice, t: Tr) -> String {
    match notice {
        Notice::Failed { op, error } => t.op_failed(op, error),
        Notice::RunStarted { label, id } => t.run_started(label, id),
        Notice::DaemonStarted(label) => t.daemon_started(label),
        Notice::Pulled(reference) => t.pulled(reference),
        Notice::PullFailed { reference, error } => t.pull_failed(reference, error),
        Notice::Pruned {
            kind,
            deleted,
            reclaimed,
        } => t.pruned(*kind, *deleted, &format_bytes(*reclaimed as f64)),
        Notice::ComposeDone { command, project } => t.compose_finished(command, project),
        Notice::Error(text) => text.clone(),
    }
}

/// The toast for a container that failed on its own.
pub fn exit_notice(exit: &ContainerExit, t: Tr) -> String {
    match exit.cause {
        ExitCause::Code(code) => t.container_exited(&exit.name, code),
        ExitCause::OutOfMemory(_) => t.container_oom(&exit.name),
        ExitCause::Unhealthy => t.container_unhealthy(&exit.name),
    }
}

pub fn is_secret_env(name: &str, value: &str) -> bool {
    secret_name(name) || url_credentials(value)
}

/// `KEY=VALUE` entries with secret-looking values replaced.
pub fn redact_env(entries: &[String]) -> Vec<String> {
    entries
        .iter()
        .map(|e| match e.split_once('=') {
            Some((name, value)) if is_secret_env(name, value) => format!("{name}={REDACTED}"),
            _ => e.clone(),
        })
        .collect()
}

/// Pretty JSON with `Config.Env` masked. Returns the input unchanged if it is not
/// container inspect JSON.
pub fn redact_inspect(json: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(json) else {
        return json.to_owned();
    };
    let Some(env) = v
        .get_mut("Config")
        .and_then(|c| c.get_mut("Env"))
        .and_then(|e| e.as_array_mut())
    else {
        return json.to_owned();
    };
    let entries: Vec<String> = env
        .iter()
        .filter_map(|e| e.as_str().map(str::to_owned))
        .collect();
    *env = redact_env(&entries).into_iter().map(Into::into).collect();
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| json.to_owned())
}

// ---- log and terminal text ----

const MAX_LINE: usize = 10_000;
/// The most a partial line's buffer keeps between lines: a rare long line does not
/// pin its size.
const KEEP_LINE_CAPACITY: usize = 1024;

#[derive(Default)]
struct Partial {
    line: String,
    /// An escape sequence split across chunks.
    escape: String,
    /// A CR was seen: the next text overwrites the line (progress bars).
    overwrite: bool,
}

/// Lines of output with ANSI escapes removed. A partial line waits for its newline;
/// the oldest lines are dropped beyond `capacity`.
pub struct LogBuffer {
    capacity: usize,
    lines: VecDeque<(LogStream, String)>,
    partial: HashMap<LogStream, Partial>,
    /// Lines finished so far, including those dropped.
    finished: u64,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            lines: VecDeque::new(),
            partial: HashMap::new(),
            finished: 0,
        }
    }

    pub fn clear(&mut self) {
        self.lines.clear();
        self.partial.clear();
    }

    pub fn lines(&self) -> impl ExactSizeIterator<Item = &(LogStream, String)> {
        self.lines.iter()
    }

    /// How many lines have been finished so far (cleared and dropped ones included).
    pub fn finished(&self) -> u64 {
        self.finished
    }

    /// The lines finished after the first `seen`, as far as they are still kept.
    pub fn since(&self, seen: u64) -> impl Iterator<Item = &(LogStream, String)> {
        let new = (self.finished - seen).min(self.lines.len() as u64) as usize;
        self.lines.iter().skip(self.lines.len() - new)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// The unfinished last line of `stream`.
    #[cfg(test)]
    pub fn partial(&self, stream: LogStream) -> &str {
        self.partial.get(&stream).map_or("", |p| p.line.as_str())
    }

    pub fn push(&mut self, stream: LogStream, text: &str) {
        let mut p = self.partial.remove(&stream).unwrap_or_default();
        // Copy the chunk only when an escape cut off in the last one has to be joined to it.
        let pending = std::mem::take(&mut p.escape);
        let joined;
        let input = if pending.is_empty() {
            text
        } else {
            joined = pending + text;
            joined.as_str()
        };
        let bytes = input.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            // Text up to the next control byte goes in as one piece. Control bytes are
            // ASCII, so `i` always stays on a character boundary.
            let end = bytes[i..]
                .iter()
                .position(|&b| is_control(b))
                .map_or(bytes.len(), |n| i + n);
            if end > i {
                p.push_text(&input[i..end]);
                i = end;
                continue;
            }
            match bytes[i] {
                0x1b => match escape_len(&input[i..]) {
                    Some(n) => i += n,
                    // Cut off: keep it for the next chunk.
                    None if bytes.len() - i < 256 => {
                        p.escape = input[i..].to_owned();
                        break;
                    }
                    // Too long to be one sequence cut off: drop it up to the end of
                    // its line, and keep the lines after it.
                    None => i += input[i..].find('\n').unwrap_or(bytes.len() - i),
                },
                b'\r' => {
                    if bytes.get(i + 1) != Some(&b'\n') {
                        p.overwrite = true;
                    }
                    i += 1;
                }
                b'\n' => {
                    p.overwrite = false;
                    // The partial line keeps its buffer for the next line; the kept line
                    // is one allocation of its exact size.
                    let line = p.line.as_str().to_owned();
                    p.line.clear();
                    if p.line.capacity() > KEEP_LINE_CAPACITY {
                        p.line = String::new();
                    }
                    self.lines.push_back((stream, line));
                    self.finished += 1;
                    if self.lines.len() > self.capacity {
                        self.lines.pop_front();
                    }
                    i += 1;
                }
                0x08 => {
                    p.line.pop();
                    i += 1;
                }
                b'\t' => {
                    p.push_text(" ");
                    i += 1;
                }
                // BEL
                _ => i += 1,
            }
        }
        self.partial.insert(stream, p);
    }
}

/// Bytes `LogBuffer::push` does not copy into a line as they are.
fn is_control(b: u8) -> bool {
    matches!(b, 0x1b | b'\r' | b'\n' | 0x08 | 0x07 | b'\t')
}

/// The length in bytes of the escape sequence at the start of `s` (which starts with
/// ESC), or `None` when it is cut off. CSI ends with a byte in `@..=~`, OSC with BEL or
/// ESC `\`; any other escape is ESC and one character.
fn escape_len(s: &str) -> Option<usize> {
    let end = |(i, c): (usize, char)| i + c.len_utf8();
    let mut chars = s.char_indices().skip(1);
    match chars.next()? {
        (_, '[') => chars.find(|(_, c)| ('@'..='~').contains(c)).map(end),
        (_, ']') => loop {
            match chars.next()? {
                bel @ (_, '\x07') => return Some(end(bel)),
                (_, '\x1b') => {
                    let next = chars.next()?;
                    if next.1 == '\\' {
                        return Some(end(next));
                    }
                }
                _ => {}
            }
        },
        other => Some(end(other)),
    }
}

impl Partial {
    /// Appends printable text, after a pending CR overwrite, up to `MAX_LINE` bytes.
    fn push_text(&mut self, s: &str) {
        if self.overwrite {
            self.line.clear();
            self.overwrite = false;
        }
        if self.line.len() + s.len() <= MAX_LINE {
            self.line.push_str(s);
        } else {
            // A character is added while the line is shorter than the cap, so the last
            // one may cross it.
            for c in s.chars() {
                if self.line.len() >= MAX_LINE {
                    break;
                }
                self.line.push(c);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::ComposeLabels;
    use crate::ui::i18n::Lang;

    fn c(name: &str, state: ContainerState, project: Option<(&str, &str)>) -> Container {
        Container {
            id: format!("{name}-id"),
            name: name.into(),
            image: format!("{name}:latest"),
            state,
            status: String::new(),
            created: 0.0,
            ports: vec![],
            compose: project.map(|(project, service)| ComposeLabels {
                project: project.into(),
                service: service.into(),
            }),
            networks: vec![],
            volumes: vec![],
        }
    }

    fn sample() -> Vec<Container> {
        vec![
            c("zeta", ContainerState::Running, None),
            c("app-web-1", ContainerState::Running, Some(("app", "web"))),
            c("app-db-1", ContainerState::Exited, Some(("app", "db"))),
            c("alpha", ContainerState::Exited, None),
        ]
    }

    #[test]
    fn tray_summary_counts_running_containers_and_each_stack() {
        let mut containers = sample();
        containers.push(c(
            "app-job-1",
            ContainerState::Restarting,
            Some(("app", "job")),
        ));
        assert_eq!(
            tray_summary(&containers),
            (3, vec![("app".to_string(), "2/3".to_string())])
        );
        assert_eq!(tray_summary(&[]), (0, vec![]));
    }

    fn names(containers: &[Container], rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                Row::Group {
                    project,
                    running,
                    total,
                    ..
                } => format!("[{project} {running}/{total}]"),
                Row::Container { id, nested } => {
                    let name = &containers.iter().find(|c| &c.id == id).unwrap().name;
                    format!("{}{name}", if *nested { "  " } else { "" })
                }
            })
            .collect()
    }

    #[test]
    fn bulk_start_and_stop_pick_the_containers_they_can_act_on() {
        use ContainerState::*;
        let all = [
            Created, Running, Paused, Restarting, Exited, Removing, Dead, Unknown,
        ];
        let pick = |f: fn(&Container) -> bool| -> Vec<ContainerState> {
            all.into_iter().filter(|s| f(&c("x", *s, None))).collect()
        };
        assert_eq!(pick(startable), [Created, Exited]);
        assert_eq!(pick(stoppable), [Running, Paused, Restarting]);
    }

    #[test]
    fn emblem_lights_running_then_paused_cells() {
        use Slot::*;
        assert_eq!(emblem_slots(&[]), [Idle; 6]);
        let mut v = sample();
        // 2 of 4 running.
        assert_eq!(
            emblem_slots(&v),
            [Running, Running, Running, Idle, Idle, Idle]
        );
        v[0].state = ContainerState::Paused;
        assert_eq!(
            emblem_slots(&v),
            [Running, Running, Paused, Paused, Idle, Idle]
        );
    }

    #[test]
    fn rows_list_singles_before_groups() {
        let v = sample();
        let rows = build_rows(&v, "", true, &BTreeSet::new());
        assert_eq!(
            names(&v, &rows),
            ["alpha", "zeta", "[app 1/2]", "  app-db-1", "  app-web-1"]
        );
    }

    #[test]
    fn rows_hide_stopped_and_empty_groups() {
        let mut v = sample();
        v[1].state = ContainerState::Exited;
        let rows = build_rows(&v, "", false, &BTreeSet::new());
        assert_eq!(names(&v, &rows), ["zeta"]);
    }

    #[test]
    fn rows_collapse_and_filter_case_insensitively() {
        let v = sample();
        let collapsed = BTreeSet::from(["app".to_owned()]);
        let rows = build_rows(&v, "APP", true, &collapsed);
        assert_eq!(names(&v, &rows), ["[app 1/2]"]);
    }

    #[test]
    fn rows_show_stopped_stacks_and_omit_projects_with_no_containers() {
        let mut v = sample();
        v[1].state = ContainerState::Exited;
        let rows = build_rows(&v, "", true, &BTreeSet::new());
        assert_eq!(
            names(&v, &rows),
            ["alpha", "zeta", "[app 0/2]", "  app-db-1", "  app-web-1"]
        );
        v.retain(|c| c.compose.is_none());
        let rows = build_rows(&v, "", true, &BTreeSet::new());
        assert_eq!(names(&v, &rows), ["alpha", "zeta"]);
    }

    #[test]
    fn env_redaction_masks_secret_names_and_url_credentials() {
        let input: Vec<String> = [
            "PATH=/usr/bin",
            "POSTGRES_PASSWORD=hunter2",
            "GITHUB_TOKEN=ghp_x",
            "AWS_SECRET_ACCESS_KEY=abc",
            "SENTRY_DSN=https://k@o.ingest/1",
            "DATABASE_URL=postgres://app:pw@db/app",
            "PUBLIC_URL=https://example.com",
            "MONKEY_COUNT=3",
            "FLAG",
        ]
        .map(String::from)
        .into();
        let r = format!("={REDACTED}");
        assert_eq!(
            redact_env(&input),
            [
                "PATH=/usr/bin".to_owned(),
                format!("POSTGRES_PASSWORD{r}"),
                format!("GITHUB_TOKEN{r}"),
                format!("AWS_SECRET_ACCESS_KEY{r}"),
                format!("SENTRY_DSN{r}"),
                format!("DATABASE_URL{r}"),
                "PUBLIC_URL=https://example.com".into(),
                "MONKEY_COUNT=3".into(),
                "FLAG".into(),
            ]
        );
    }

    #[test]
    fn inspect_redaction_touches_only_container_env() {
        let json = r#"{"Id":"abc","Config":{"Env":["A=1","API_KEY=s3cr3t"],"Image":"nginx"}}"#;
        let out: serde_json::Value = serde_json::from_str(&redact_inspect(json)).unwrap();
        assert_eq!(
            out["Config"]["Env"],
            serde_json::json!(["A=1", "API_KEY=<redacted>"])
        );
        assert_eq!(out["Config"]["Image"], "nginx");
        let image = r#"{"Id":"sha256:1","RepoTags":["x"]}"#;
        assert_eq!(redact_inspect(image), image);
        assert_eq!(redact_inspect("not json"), "not json");
    }

    #[test]
    fn log_buffer_hands_out_only_lines_not_seen_yet() {
        let mut b = LogBuffer::new(3);
        b.push(LogStream::Stdout, "a\nb\npart");
        let seen = b.finished();
        assert_eq!(seen, 2);
        b.push(LogStream::Stdout, "ial\nc\n");
        let new: Vec<_> = b.since(seen).map(|(_, l)| l.as_str()).collect();
        assert_eq!(new, ["partial", "c"]);
        // More new lines than are kept: only the kept ones.
        b.push(LogStream::Stdout, "d\ne\nf\ng\n");
        let new: Vec<_> = b.since(b.finished() - 4).map(|(_, l)| l.as_str()).collect();
        assert_eq!(new, ["e", "f", "g"]);
        // Cleared: nothing new until the next line.
        b.clear();
        assert_eq!(b.since(b.finished()).count(), 0);
    }

    #[test]
    fn log_buffer_splits_lines_and_strips_escapes() {
        let mut b = LogBuffer::new(3);
        b.push(LogStream::Stdout, "a\x1b[31mred\x1b[0m\npart");
        assert_eq!(b.len(), 1);
        assert_eq!(b.partial(LogStream::Stdout), "part");
        // An escape cut across chunks, CR overwrite, and CRLF.
        b.push(LogStream::Stdout, "ial\x1b[");
        b.push(LogStream::Stdout, "1m!\r\n10%\r50%\n");
        let lines: Vec<&str> = b.lines().map(|(_, l)| l.as_str()).collect();
        assert_eq!(lines, ["ared", "partial!", "50%"]);
        b.push(LogStream::Stderr, "x\n");
        assert_eq!(b.len(), 3, "oldest line dropped beyond capacity");
        assert_eq!(b.lines().next().unwrap().1, "partial!");
    }

    #[test]
    fn log_buffer_edge_cases() {
        let lines = |b: &LogBuffer| -> Vec<String> { b.lines().map(|(_, l)| l.clone()).collect() };
        let mut b = LogBuffer::new(100);
        // Tabs become spaces; BEL is dropped; backspace removes a whole character.
        b.push(LogStream::Stdout, "a\tb\x07c日\x08d\n");
        // Overwrite after CR starts with the next printable character, a tab included.
        b.push(LogStream::Stdout, "old\r\tnew\n");
        // CR at the end of a chunk and LF at the start of the next one end the line.
        b.push(LogStream::Stdout, "crlf\r");
        b.push(LogStream::Stdout, "\nnext\n");
        // OSC ends with BEL or ESC \; any other escape drops only ESC and the next character.
        b.push(
            LogStream::Stdout,
            "\x1b]0;title\x07t1\x1b]8;;x\x1b\\t2\x1b(Bt3\n",
        );
        // An OSC cut across chunks, with an ESC that is not a terminator inside it.
        b.push(LogStream::Stdout, "o\x1b]0;a\x1bb");
        b.push(LogStream::Stdout, "c\x07k\n");
        // Other control characters are kept.
        b.push(LogStream::Stdout, "\x00\x01z\n");
        assert_eq!(
            lines(&b),
            [
                "a bcd",
                " new",
                "crlf",
                "next",
                "t1t2Bt3",
                "ok",
                "\x00\x01z"
            ]
        );
        // Streams keep their own partial lines and escapes.
        b.push(LogStream::Stdout, "out\x1b[3");
        b.push(LogStream::Stderr, "err");
        b.push(LogStream::Stdout, "1m!\n");
        b.push(LogStream::Stderr, "!\n");
        let tail: Vec<_> = b.lines().skip(7).cloned().collect();
        assert_eq!(
            tail,
            [
                (LogStream::Stdout, "out!".to_owned()),
                (LogStream::Stderr, "err!".to_owned())
            ]
        );
    }

    #[test]
    fn log_buffer_caps_lines_and_cut_escapes() {
        let mut b = LogBuffer::new(10);
        // A line stops growing at MAX_LINE bytes; a multi-byte character may cross it.
        let long = "x".repeat(MAX_LINE - 1) + "日本語";
        b.push(LogStream::Stdout, &format!("{long}\n"));
        b.push(LogStream::Stdout, &"y".repeat(MAX_LINE * 2));
        b.push(LogStream::Stdout, "zz\n");
        let got: Vec<&str> = b.lines().map(|(_, l)| l.as_str()).collect();
        assert_eq!(got[0], "x".repeat(MAX_LINE - 1) + "日");
        assert_eq!(got[1], "y".repeat(MAX_LINE));
        // Overwrite still clears a full line.
        b.push(LogStream::Stdout, &"y".repeat(MAX_LINE));
        b.push(LogStream::Stdout, "\rshort\n");
        assert_eq!(b.lines().last().unwrap().1, "short");
        // An unfinished escape longer than 256 bytes is dropped, not kept for the next chunk.
        b.push(LogStream::Stdout, &format!("a\x1b[{}", "1;".repeat(200)));
        b.push(LogStream::Stdout, "b\n");
        assert_eq!(b.lines().last().unwrap().1, "ab");
        // Only to the end of its line: the lines after it in the same chunk are kept.
        b.push(
            LogStream::Stdout,
            &format!("e\x1b]0;{}\nnext\nlast\n", "t".repeat(300)),
        );
        let got: Vec<&str> = b.lines().map(|(_, l)| l.as_str()).collect();
        assert_eq!(got[got.len() - 3..], ["e", "next", "last"]);
        // A short one is kept and finished by the next chunk.
        b.push(LogStream::Stdout, "c\x1b[1;3");
        b.push(LogStream::Stdout, "1md\n");
        assert_eq!(b.lines().last().unwrap().1, "cd");
    }

    #[test]
    fn log_buffer_lines_are_exact_size_and_carry_nothing_over() {
        let mut b = LogBuffer::new(10);
        // Pieces between escapes would grow a line past its length; a kept line is not.
        b.push(
            LogStream::Stdout,
            "\x1b[2mtime\x1b[0m \x1b[32mINFO\x1b[0m \x1b[1mworker\x1b[0m: done\n",
        );
        // The buffer reused after a long line, a CR overwrite and a backspace starts empty.
        b.push(LogStream::Stdout, &format!("{}\nab\r", "x".repeat(4000)));
        b.push(LogStream::Stdout, "cd\nef\x08g\n\n");
        let got: Vec<&str> = b.lines().map(|(_, l)| l.as_str()).collect();
        assert_eq!(
            got,
            ["time INFO worker: done", &"x".repeat(4000), "cd", "eg", ""]
        );
        for (_, l) in b.lines() {
            assert_eq!(l.capacity(), l.len(), "{l:?}");
        }
        assert!(
            b.partial
                .values()
                .all(|p| p.line.capacity() <= KEEP_LINE_CAPACITY)
        );
    }

    #[test]
    fn conn_view_offers_start_only_when_the_socket_looks_down() {
        let t = Tr(Lang::En);
        let target = Target {
            host: "unix:///var/run/docker.sock".into(),
            source: "default".into(),
            context: None,
        };
        let daemon = DaemonStatus {
            startable: Some("Docker Desktop".into()),
            starting: None,
            ready_timeout_secs: 60,
        };
        let err = |kind| crate::dto::UiError {
            kind,
            message: "boom".into(),
        };
        let down = ConnState::Unreachable {
            error: err(UiErrorKind::SocketMissing),
            retry_in: 4,
        };
        let v = describe_conn(&down, &target, &daemon, t);
        assert_eq!(v.tone, Tone::Error);
        assert_eq!(v.start.as_deref(), Some("Start Docker Desktop"));
        assert_eq!(v.detail, "boom Retrying in 4s.");
        let slow = ConnState::Unreachable {
            error: err(UiErrorKind::Daemon),
            retry_in: 4,
        };
        assert_eq!(describe_conn(&slow, &target, &daemon, t).start, None);
        let starting = DaemonStatus {
            starting: Some("Docker Desktop".into()),
            ..daemon
        };
        let v = describe_conn(&down, &target, &starting, t);
        assert_eq!(v.starting, Some(("Docker Desktop".into(), 60)));
    }

    #[test]
    fn usage_counts_count_each_container_once_per_name() {
        let mut v = sample();
        v[0].volumes = vec!["data".into(), "cache".into(), "data".into()];
        v[1].volumes = vec!["data".into()];
        v[2].volumes = vec![];
        v[3].volumes = vec!["cache".into(), "cache".into()];
        let counts = usage_counts(&v, |c| &c.volumes);
        assert_eq!(
            counts,
            HashMap::from([("data", 2), ("cache", 2)]),
            "a name a container mounts twice counts once"
        );
        assert!(usage_counts(&[], |c| &c.networks).is_empty());
    }

    #[test]
    fn meters_format_usage() {
        let u = SystemUsage {
            cpu_used_cores: Some(1.5),
            cpu_cores: Some(8),
            mem_usage: Some(2e9),
            mem_total: Some(8e9),
            disk_usage: None,
            disk_total: None,
            disk_breakdown: None,
        };
        let [cpu, mem, disk] = meters(&u, Tr(Lang::En));
        assert_eq!(
            (cpu.amount.as_str(), cpu.percent.as_str()),
            ("1.50 / 8 cores", "18.8%")
        );
        let [cpu, ..] = meters(&u, Tr(Lang::Ja));
        assert_eq!(cpu.amount, "1.50 / 8 コア");
        let one = SystemUsage {
            cpu_cores: Some(1),
            ..u
        };
        assert_eq!(meters(&one, Tr(Lang::En))[0].amount, "1.50 / 1 core");
        assert_eq!(
            (mem.amount.as_str(), mem.fraction),
            ("2.0 GB / 8.0 GB", Some(0.25))
        );
        assert_eq!((disk.amount.as_str(), disk.fraction), ("—", None));
    }

    #[test]
    fn volume_prune_scope_follows_the_api_version() {
        assert!(prune_removes_named_volumes(Some("1.41")));
        assert!(!prune_removes_named_volumes(Some("1.42")));
        assert!(!prune_removes_named_volumes(Some("1.47")));
        assert!(!prune_removes_named_volumes(None));
    }

    #[test]
    fn formats_ids_and_times() {
        assert_eq!(short_id("sha256:0123456789abcdef"), "0123456789ab");
        let t = Tr(Lang::En);
        assert_eq!(relative_time(100.0, 130.0, t), "just now");
        assert_eq!(relative_time(0.0, 7_200.0, t), "2 hours ago");
    }

    #[test]
    fn skipped_settings_read_like_the_cli() {
        assert_eq!(skipped_settings(&[]), None);
        let list = skipped_settings(&[
            SkippedSetting::Mount {
                kind: "bind".into(),
                source: Some("/home/me/site".into()),
                target: "/srv".into(),
            },
            SkippedSetting::Mount {
                kind: "tmpfs".into(),
                source: None,
                target: "/run".into(),
            },
            SkippedSetting::Entrypoint("/bin/sh -c".into()),
            SkippedSetting::Entrypoint(String::new()),
            SkippedSetting::NetworkMode("container:db".into()),
            SkippedSetting::Env("CERT".into()),
            SkippedSetting::Option("--memory 512m".into()),
        ]);
        assert_eq!(
            list.as_deref(),
            Some(
                "bind /home/me/site → /srv, tmpfs /run, --entrypoint /bin/sh -c, --entrypoint \"\", --network container:db, -e CERT, --memory 512m"
            )
        );
    }

    #[test]
    fn a_check_marks_containers_with_a_newer_image_and_counts_them() {
        let t = Tr(Lang::En);
        assert_eq!(
            freshness_note(Some(&Freshness::Available), t),
            t.update_available()
        );
        assert_eq!(
            freshness_note(Some(&Freshness::Pulled), t),
            t.update_pulled()
        );
        assert_eq!(freshness_note(Some(&Freshness::Current), t), "");
        assert_eq!(freshness_note(Some(&Freshness::Unknown("x".into())), t), "");
        assert_eq!(freshness_note(None, t), "");

        let check = |container: &str, freshness| UpdateCheck {
            container: container.into(),
            freshness,
        };
        let checks = [
            check("a", Freshness::Available),
            check("b", Freshness::Pulled),
            check("c", Freshness::Current),
            check("d", Freshness::Unknown("denied".into())),
        ];
        assert_eq!(
            update_check_summary(&checks, t),
            t.update_check_result(2, 1)
        );
        assert_eq!(update_check_summary(&[], t), t.update_check_result(0, 0));
    }

    #[test]
    fn network_choices_join_what_it_is_not_on_and_leave_what_it_is() {
        let net = |name: &str| Network {
            id: format!("{name}-id"),
            name: name.into(),
            driver: "bridge".into(),
            scope: "local".into(),
            internal: false,
            builtin: matches!(name, "bridge" | "host" | "none"),
        };
        let networks = [
            net("bridge"),
            net("host"),
            net("none"),
            net("front"),
            net("back"),
        ];
        let mut web = c("web", ContainerState::Running, None);
        web.networks = vec!["front".into()];
        assert_eq!(
            network_choices(&web, &networks),
            [
                NetworkChoice::Connect("back".into()),
                NetworkChoice::Connect("bridge".into()),
                NetworkChoice::Disconnect("front".into()),
            ]
        );
        // The host's network, none, or another container's stack (no networks listed).
        for on in [vec!["host".to_owned()], vec!["none".to_owned()], vec![]] {
            web.networks = on;
            assert!(network_choices(&web, &networks).is_empty());
        }
    }

    #[test]
    fn update_all_takes_the_standalone_containers_with_a_newer_image() {
        let containers = vec![
            c("zeta", ContainerState::Running, None),
            c("alpha", ContainerState::Exited, None),
            c("current", ContainerState::Running, None),
            c("app-web-1", ContainerState::Running, Some(("app", "web"))),
        ];
        let updates = HashMap::from([
            ("zeta-id".to_owned(), Freshness::Available),
            ("alpha-id".to_owned(), Freshness::Pulled),
            ("current-id".to_owned(), Freshness::Current),
            ("app-web-1-id".to_owned(), Freshness::Available),
            ("gone-id".to_owned(), Freshness::Available),
        ]);
        assert_eq!(
            update_targets(&containers, &updates),
            [
                ("alpha-id".to_owned(), "alpha".to_owned()),
                ("zeta-id".to_owned(), "zeta".to_owned())
            ]
        );
        assert!(update_targets(&containers, &HashMap::new()).is_empty());
    }

    #[test]
    fn update_is_offered_outside_compose_and_says_what_it_did() {
        assert!(updatable(&c("web", ContainerState::Running, None)));
        assert!(!updatable(&c(
            "shop-web-1",
            ContainerState::Running,
            Some(("shop", "web"))
        )));

        let t = Tr(crate::ui::i18n::Lang::En);
        let mut u = ContainerUpdate {
            name: "web".into(),
            outcome: UpdateOutcome::UpToDate,
        };
        assert_eq!(update_notice(&u, t).1, t.update_up_to_date("web"));
        u.outcome = UpdateOutcome::Updated;
        assert_eq!(update_notice(&u, t).1, t.updated("web"));
        u.outcome = UpdateOutcome::UpdatedKeptOld("web-replaced-1".into());
        assert_eq!(
            update_notice(&u, t),
            (
                NoticeKind::Success,
                t.updated_kept_old("web", "web-replaced-1")
            )
        );
        u.outcome = UpdateOutcome::NotRestored {
            error: "port is already allocated".into(),
            kept_as: "web-replaced-1".into(),
            stopped: true,
        };
        let (kind, text) = update_notice(&u, t);
        assert_eq!(kind, NoticeKind::Error);
        assert!(text.contains("web-replaced-1, stopped"), "{text}");
        assert!(text.contains("port is already allocated"), "{text}");
    }

    #[test]
    fn backend_notices_are_worded_in_the_ui_language() {
        use crate::dto::{ContainerAction, Op, PruneKind};
        use crate::ui::i18n::Lang;
        let failed = Notice::Failed {
            op: Op::RemoveVolume("data".into()),
            error: "volume is in use".into(),
        };
        assert_eq!(
            notice_text(&failed, Tr(Lang::En)),
            "Remove volume data failed: volume is in use"
        );
        assert_eq!(
            notice_text(&failed, Tr(Lang::Ja)),
            "ボリューム data の削除に失敗しました: volume is in use"
        );
        let stop = Notice::Failed {
            op: Op::Container(ContainerAction::Stop),
            error: "gone".into(),
        };
        assert_eq!(notice_text(&stop, Tr(Lang::En)), "Stop failed: gone");
        let pruned = Notice::Pruned {
            kind: PruneKind::Images,
            deleted: 3,
            reclaimed: 2048,
        };
        assert_eq!(
            notice_text(&pruned, Tr(Lang::Ja)),
            format!(
                "dangling イメージを 3 件削除しました（{} 回収）",
                format_bytes(2048.0)
            )
        );
        assert_eq!(
            notice_text(&Notice::Pulled("nginx:1.27".into()), Tr(Lang::Ja)),
            "nginx:1.27 を Pull しました"
        );
        assert_eq!(pruned.kind(), NoticeKind::Success);
        assert_eq!(failed.kind(), NoticeKind::Error);
    }

    #[test]
    fn an_out_of_memory_kill_is_told_apart_from_an_exit_code() {
        let t = Tr(crate::ui::i18n::Lang::En);
        let mut exit = ContainerExit {
            id: "c1".into(),
            name: "web".into(),
            cause: ExitCause::OutOfMemory(137),
        };
        assert_eq!(exit_notice(&exit, t), t.container_oom("web"));
        exit.cause = ExitCause::Code(137);
        assert_eq!(exit_notice(&exit, t), t.container_exited("web", 137));
        exit.cause = ExitCause::Unhealthy;
        assert_eq!(exit_notice(&exit, t), t.container_unhealthy("web"));
    }

    #[test]
    fn cleanup_form_enforces_hierarchy_and_legacy_volume_safety() {
        let mut form = CleanupForm::new(Some("1.52"));
        assert!(!form.legacy_volumes);
        assert!(form.volumes);
        assert!(!form.warns_named_volumes());
        assert_eq!(
            form.scope(),
            CleanupScope {
                containers: true,
                networks: true,
                images: Some(CleanupImages::Dangling),
                volumes: Some(CleanupVolumes::Anonymous),
                build_cache: true,
            }
        );

        // Checking child enables parent; unchecking parent clears child.
        form.set(CleanupToggle::DanglingImages, false);
        assert_eq!(form.scope().images, None);
        form.set(CleanupToggle::TaggedImages, true);
        assert!(form.dangling_images && form.tagged_images);
        assert_eq!(form.scope().images, Some(CleanupImages::AllUnused));
        form.set(CleanupToggle::DanglingImages, false);
        assert!(!form.tagged_images);

        form.set(CleanupToggle::NamedVolumes, true);
        assert!(form.volumes && form.named_volumes);
        assert!(form.warns_named_volumes());
        assert_eq!(form.scope().volumes, Some(CleanupVolumes::AllUnused));

        // Switching to a legacy (< 1.42) daemon clears volumes to OFF so named volumes stay safe.
        let mut fresh = CleanupForm::new(Some("1.52"));
        fresh.sync_api(Some("1.41"));
        assert!(fresh.legacy_volumes);
        assert!(!fresh.volumes);
        assert_eq!(fresh.scope().volumes, None);
        fresh.set(CleanupToggle::Volumes, true);
        assert!(fresh.warns_named_volumes());
        assert_eq!(fresh.scope().volumes, Some(CleanupVolumes::AllUnused));
    }

    #[test]
    fn cleanup_preview_reflects_resources_freed_by_stopped_containers_and_formats_notices() {
        let t = Tr(crate::ui::i18n::Lang::En);
        let mut running = c("web", ContainerState::Running, None);
        running.networks = vec!["live_net".into()];
        running.volumes = vec!["live_vol".into()];

        let mut stopped = c("old", ContainerState::Exited, None);
        stopped.networks = vec!["old_net".into()];
        stopped.volumes = vec!["anon_old".into(), "named_old".into()];

        let paused = c("paused", ContainerState::Paused, None);

        let snap = Snapshot {
            revision: 1,
            conn: ConnState::Connected {
                info: crate::dto::EngineInfo {
                    version: "29.0".into(),
                    api_version: "1.52".into(),
                    os: "linux".into(),
                    arch: "arm64".into(),
                },
            },
            target: Target {
                host: "unix:///var/run/docker.sock".into(),
                source: "test".into(),
                context: None,
            },
            containers: vec![running, stopped, paused],
            images: vec![
                crate::dto::Image {
                    id: "sha256:1".into(),
                    short_id: "1".into(),
                    tags: vec![],
                    size: 10e6,
                    created: 0.0,
                    containers: 0.0,
                    dangling: true,
                },
                crate::dto::Image {
                    id: "sha256:2".into(),
                    short_id: "2".into(),
                    tags: vec!["unused:1".into()],
                    size: 25e6,
                    created: 0.0,
                    containers: 0.0,
                    dangling: false,
                },
            ],
            volumes: vec![
                Volume {
                    name: "live_vol".into(),
                    driver: "local".into(),
                    mountpoint: String::new(),
                    created_at: None,
                    compose_project: None,
                    anonymous: true,
                    size: Some(5e6),
                },
                Volume {
                    name: "anon_old".into(),
                    driver: "local".into(),
                    mountpoint: String::new(),
                    created_at: None,
                    compose_project: None,
                    anonymous: true,
                    size: Some(8e6),
                },
                Volume {
                    name: "named_old".into(),
                    driver: "local".into(),
                    mountpoint: String::new(),
                    created_at: None,
                    compose_project: Some("oldstack".into()),
                    anonymous: false,
                    size: Some(40e6),
                },
            ],
            networks: vec![
                Network {
                    id: "n0".into(),
                    name: "bridge".into(),
                    driver: "bridge".into(),
                    scope: "local".into(),
                    internal: false,
                    builtin: true,
                },
                Network {
                    id: "n1".into(),
                    name: "live_net".into(),
                    driver: "bridge".into(),
                    scope: "local".into(),
                    internal: false,
                    builtin: false,
                },
                Network {
                    id: "n2".into(),
                    name: "old_net".into(),
                    driver: "bridge".into(),
                    scope: "local".into(),
                    internal: false,
                    builtin: false,
                },
            ],
            pending: vec![],
            pulls: vec![],
            compose_runs: vec![],
            compose_available: Some(true),
            usage: SystemUsage {
                disk_breakdown: Some(crate::dto::DiskBreakdown {
                    build_cache: 12e6,
                    ..Default::default()
                }),
                ..Default::default()
            },
            daemon: DaemonStatus {
                startable: None,
                starting: None,
                ready_timeout_secs: 60,
            },
        };

        let mut form = CleanupForm::new(Some("1.52"));
        let p_with_containers = cleanup_preview(&snap, &form, t);
        // Paused is excluded (only `old` is stopped); `old_net`, `anon_old` and `named_old` are freed!
        assert_eq!(p_with_containers.containers, "1 item");
        assert_eq!(p_with_containers.networks, "1 item");
        assert_eq!(p_with_containers.volumes, "1 item · 8.0 MB");
        assert_eq!(p_with_containers.named_volumes, "1 item · 40.0 MB");
        assert_eq!(p_with_containers.dangling_images, "1 item · 10.0 MB");
        assert_eq!(p_with_containers.tagged_images, "1 item · 25.0 MB");
        assert_eq!(p_with_containers.build_cache, "up to 12.0 MB");
        assert!(p_with_containers.can_submit);

        // When stopped containers are NOT selected, `old` still holds `old_net`, `anon_old`, `named_old`.
        form.set(CleanupToggle::Containers, false);
        let p_keep_containers = cleanup_preview(&snap, &form, t);
        assert_eq!(p_keep_containers.networks, "0 items");
        assert_eq!(p_keep_containers.volumes, "0 items");
        assert_eq!(p_keep_containers.named_volumes, "0 items");

        let ok = CleanupReport {
            containers_deleted: 2,
            reclaimed: 15e6,
            ..Default::default()
        };
        assert_eq!(cleanup_notice(&ok, t).0, NoticeKind::Success);
        let partial = CleanupReport {
            containers_deleted: 1,
            reclaimed: 5e6,
            partial_error: Some((crate::dto::CleanupStep::Volumes, "in use".into())),
            ..Default::default()
        };
        let (kind, text) = cleanup_notice(&partial, t);
        assert_eq!(kind, NoticeKind::Error);
        assert!(text.contains("in use"));
    }
}
