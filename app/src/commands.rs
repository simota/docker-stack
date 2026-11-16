//! The fixed set of operations the UI may run (design doc §4). There is no generic
//! Docker API passthrough: the socket is root-equivalent.

use engine::{ExecInput, ResourceKind, TtySize};

use crate::backend::{Backend, UiEvent};
use crate::config::Config;
use crate::dto::{
    CleanupScope, ComposeCommand, ContainerAction, ContainerUpdate, ContextEntry, ExecShell,
    InspectKind, Notice, Op, PruneKind, RunForm, RunTemplate, SkippedSetting, UiError, UiErrorKind,
    UpdateCheck, UpdateOutcome,
};
use crate::{run_spec, state};

type R<T> = Result<T, UiError>;

pub(crate) fn invalid(message: impl Into<String>) -> UiError {
    UiError {
        kind: UiErrorKind::Local,
        message: message.into(),
    }
}

/// Reconnects immediately, skipping any backoff wait.
pub fn retry_now(app: &Backend) {
    app.state().reset_backoff();
    state::connect(app, None);
}

/// Starts the local Docker daemon behind the current target (when the app knows how).
pub fn start_daemon(app: &Backend) -> R<()> {
    state::start_daemon(app)
}

/// Re-lists every resource kind.
pub fn refresh(app: &Backend) {
    state::refresh_all(app);
}

pub async fn list_contexts() -> R<Vec<ContextEntry>> {
    let entries = tokio::task::spawn_blocking(engine::list_contexts)
        .await
        .map_err(|e| invalid(format!("Could not list Docker contexts: {e}")))?;
    Ok(entries
        .into_iter()
        .map(|c| ContextEntry {
            name: c.name,
            host: c.host,
            current: c.current,
            supported: c.supported,
        })
        .collect())
}

fn resolve_target_for_context(
    find: impl FnOnce(&str) -> Option<engine::ContextEntry>,
    default_target: impl FnOnce() -> engine::ConnectTarget,
    name: Option<&str>,
) -> R<engine::ConnectTarget> {
    match name {
        Some(n) => {
            let entry = find(n).ok_or_else(|| invalid(format!("Unknown context {n}")))?;
            if !entry.supported {
                return Err(invalid(format!(
                    "Context {n} ({}) uses an unsupported transport.",
                    entry.host
                )));
            }
            Ok(engine::ConnectTarget::from(&entry))
        }
        None => Ok(default_target()),
    }
}

/// Switches to a Docker context (or `None` to follow the CLI default) and remembers it.
pub async fn use_context(app: Backend, name: Option<String>) -> R<Config> {
    let lookup_name = name.clone();
    let target = tokio::task::spawn_blocking(move || {
        resolve_target_for_context(
            engine::find_context,
            engine::resolve_default_target,
            lookup_name.as_deref(),
        )
    })
    .await
    .map_err(|e| invalid(format!("Could not resolve context: {e}")))??;

    let saved = app
        .config()
        .update_with_save(|c| c.context = name, |c| c.save())
        .await
        .map_err(|e| invalid(format!("Could not save settings: {e}")))?;
    state::connect(&app, Some(target));
    Ok(saved)
}

pub fn container_action(app: &Backend, id: String, action: ContainerAction) -> R<()> {
    let engine = app.state().engine()?;
    let kinds: &'static [ResourceKind] = match action {
        ContainerAction::Remove { .. } => &[ResourceKind::Container, ResourceKind::Image],
        _ => &[ResourceKind::Container],
    };
    let target = id.clone();
    let start = move || engine.container_action(&target, action.into());
    state::run_op(app, id, kinds, Op::Container(action), start, |_, ()| {})
}

/// Updates container `id` (named `name`): pulls the image it was created from and,
/// when its tag has moved, replaces it with a container made from the newer image,
/// keeping its settings, volumes and networks; a failed step puts the original back.
/// Containers of a compose project are refused (their stack's Pull and Up do this).
pub fn update_container(app: &Backend, id: String, name: String) -> R<()> {
    let engine = app.state().engine()?;
    let target = id.clone();
    let start = move || {
        let task = engine.update_container(&target);
        async move { update_outcome(task.await) }
    };
    state::run_op(
        app,
        id,
        &[ResourceKind::Container, ResourceKind::Image],
        Op::Update,
        start,
        move |app, outcome| {
            app.send(UiEvent::ContainerUpdated(ContainerUpdate { name, outcome }));
        },
    )
}

/// An Update's result for the UI. One that failed and left the original elsewhere is an
/// outcome to tell, not only an error: the person has to know where the original is.
fn update_outcome(
    result: Result<engine::UpdateOutcome, engine::EngineError>,
) -> Result<UpdateOutcome, engine::EngineError> {
    match result {
        Ok(outcome) => Ok(outcome.into()),
        Err(engine::EngineError::NotRestored {
            error,
            kept_as,
            stopped,
        }) => Ok(UpdateOutcome::NotRestored {
            error: error.to_string(),
            kept_as,
            stopped,
        }),
        Err(e) => Err(e),
    }
}

/// Connects container `id` to `network` (`connect`), or disconnects it from it.
pub fn container_network(app: &Backend, id: String, network: String, connect: bool) -> R<()> {
    let engine = app.state().engine()?;
    let what = if connect {
        Op::ConnectNetwork(network.clone())
    } else {
        Op::DisconnectNetwork(network.clone())
    };
    let key = format!("network:{id}");
    let start = move || {
        if connect {
            engine.connect_network(&network, &id)
        } else {
            engine.disconnect_network(&network, &id)
        }
    };
    state::run_op(
        app,
        key,
        &[ResourceKind::Container, ResourceKind::Network],
        what,
        start,
        |_, ()| {},
    )
}

/// Updates every container in `containers` (id, name) like [`update_container`];
/// one that is refused does not stop the rest.
pub fn update_containers(app: &Backend, containers: Vec<(String, String)>) -> R<()> {
    each(containers, |(id, name)| update_container(app, id, name))
}

/// Checks every listed container for a newer image of the tag it was created from
/// (the daemon asks the registry; nothing is pulled).
pub async fn check_updates(app: Backend) -> R<Vec<UpdateCheck>> {
    let engine = app.state().engine()?;
    let ids = app
        .snapshot()
        .containers
        .into_iter()
        .map(|c| c.id)
        .collect();
    let checks = engine.check_updates(ids).await?;
    Ok(checks
        .into_iter()
        .map(|c| UpdateCheck {
            container: c.container,
            freshness: c.freshness.into(),
        })
        .collect())
}

/// Runs `action` on every container in `ids` (Start, Stop or Restart on a selection);
/// one that refuses does not stop the rest.
pub fn container_actions(app: &Backend, ids: Vec<String>, action: ContainerAction) -> R<()> {
    each(ids, |id| container_action(app, id, action))
}

/// Creates and starts a container from the Run form (`docker run -d`).
pub fn run_container(app: &Backend, form: &RunForm) -> R<()> {
    let spec = run_spec::parse(form)?;
    let engine = app.state().engine()?;
    let image = spec.image.clone();
    let label = spec.name.clone().unwrap_or_else(|| image.clone());
    let start = move || engine.run_container(spec);
    state::run_op(
        app,
        format!("run:{label}"),
        &[ResourceKind::Container, ResourceKind::Volume],
        Op::Run(image),
        start,
        move |app, id: String| {
            let short = id[..id.len().min(12)].to_owned();
            state::notice(app, Notice::RunStarted { label, id: short });
        },
    )
}

/// A Run form filled in from container `id`'s settings ("Duplicate"), named so it
/// does not clash with an existing container. Nothing is created: the form goes
/// through the same checks as one typed by hand when it is run.
pub async fn container_template(app: Backend, id: String) -> R<RunTemplate> {
    let engine = app.state().engine()?;
    let mut template = engine.container_template(&id).await?;
    // A value the environment box cannot hold as one line is left out and listed,
    // rather than changed.
    let (env, lossy): (Vec<String>, Vec<String>) = std::mem::take(&mut template.spec.env)
        .into_iter()
        .partition(|e| run_spec::env_round_trips(e));
    template.spec.env = env;
    // So is a volume whose path the volume box would split.
    let (volumes, split): (Vec<_>, Vec<_>) = std::mem::take(&mut template.spec.volumes)
        .into_iter()
        .partition(run_spec::volume_round_trips);
    template.spec.volumes = volumes;
    let mut form = run_spec::form_from_spec(&template.spec);
    let taken = app.state().container_names();
    form.name = copy_name(&form.name, &taken);
    let mut skipped: Vec<SkippedSetting> = template.skipped.into_iter().map(Into::into).collect();
    skipped.extend(lossy.iter().map(|e| SkippedSetting::Env(env_key(e))));
    skipped.extend(split.into_iter().map(|v| SkippedSetting::Mount {
        kind: "volume".into(),
        source: Some(v.volume),
        target: v.target,
    }));
    Ok(RunTemplate {
        source: template.spec.name.unwrap_or(id),
        form,
        skipped,
    })
}

/// The name of a `KEY=VALUE` entry (never its value, which may be a secret).
fn env_key(entry: &str) -> String {
    entry
        .split_once('=')
        .map_or(entry, |(k, _)| k)
        .trim()
        .to_owned()
}

/// `{name}-copy`, or `{name}-copy-2` and up when that is taken too. Empty when the
/// original has no name (the daemon then picks one).
fn copy_name(name: &str, taken: &[String]) -> String {
    if name.is_empty() {
        return String::new();
    }
    let free = |n: &String| !taken.contains(n);
    std::iter::once(format!("{name}-copy"))
        .chain((2..).map(|i| format!("{name}-copy-{i}")))
        .find(free)
        .unwrap_or_default()
}

pub async fn inspect(app: Backend, kind: InspectKind, id: String) -> R<String> {
    let engine = app.state().engine()?;
    let json = match kind {
        InspectKind::Container => engine.inspect_container(&id).await?,
        InspectKind::Image => engine.inspect_image(&id).await?,
        InspectKind::Volume => engine.inspect_volume(&id).await?,
        InspectKind::Network => engine.inspect_network(&id).await?,
    };
    Ok(json)
}

pub fn remove_image(app: &Backend, id: String, force: bool) -> R<()> {
    let engine = app.state().engine()?;
    let target = id.clone();
    let start = move || engine.remove_image(&target, force);
    state::run_op(
        app,
        id,
        &[ResourceKind::Image],
        Op::RemoveImage,
        start,
        |_, ()| {},
    )
}

pub fn remove_volume(app: &Backend, name: String) -> R<()> {
    let engine = app.state().engine()?;
    let what = Op::RemoveVolume(name.clone());
    let target = name.clone();
    let start = move || engine.remove_volume(&target);
    state::run_op(app, name, &[ResourceKind::Volume], what, start, |_, ()| {})
}

/// Runs `op` on every item as its own operation, so one refusal (say, a volume still
/// mounted) does not stop the rest. Returns the first error.
fn each<T>(items: Vec<T>, mut op: impl FnMut(T) -> R<()>) -> R<()> {
    let mut first = None;
    for item in items {
        if let Err(e) = op(item) {
            first.get_or_insert(e);
        }
    }
    first.map_or(Ok(()), Err)
}

pub fn remove_volumes(app: &Backend, names: Vec<String>) -> R<()> {
    each(names, |name| remove_volume(app, name))
}

/// Removes containers with their anonymous volumes; `force` stops a running one.
pub fn remove_containers(app: &Backend, containers: Vec<(String, bool)>) -> R<()> {
    each(containers, |(id, force)| {
        container_action(
            app,
            id,
            ContainerAction::Remove {
                force,
                volumes: true,
            },
        )
    })
}

pub fn remove_images(app: &Backend, images: Vec<(String, bool)>) -> R<()> {
    each(images, |(id, force)| remove_image(app, id, force))
}

pub fn remove_networks(app: &Backend, ids: Vec<String>) -> R<()> {
    each(ids, |id| remove_network(app, id))
}

pub fn remove_network(app: &Backend, id: String) -> R<()> {
    let engine = app.state().engine()?;
    let target = id.clone();
    let start = move || engine.remove_network(&target);
    state::run_op(
        app,
        id,
        &[ResourceKind::Network],
        Op::RemoveNetwork,
        start,
        |_, ()| {},
    )
}

pub(crate) fn valid_name(name: &str) -> R<String> {
    let name = name.trim();
    let ok = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c));
    if ok {
        Ok(name.to_owned())
    } else {
        Err(invalid(
            "Names must start with a letter or digit and use only letters, digits, '_', '.' or '-'.",
        ))
    }
}

pub fn create_volume(app: &Backend, name: &str) -> R<()> {
    let name = valid_name(name)?;
    let engine = app.state().engine()?;
    let (key, what) = (format!("new-volume:{name}"), Op::CreateVolume(name.clone()));
    let start = move || engine.create_volume(&name);
    state::run_op(app, key, &[ResourceKind::Volume], what, start, |_, ()| {})
}

pub fn create_network(app: &Backend, name: &str) -> R<()> {
    let name = valid_name(name)?;
    let engine = app.state().engine()?;
    let (key, what) = (
        format!("new-network:{name}"),
        Op::CreateNetwork(name.clone()),
    );
    let start = move || engine.create_network(&name);
    state::run_op(app, key, &[ResourceKind::Network], what, start, |_, ()| {})
}

pub const PRUNE_PENDING_KEY: &str = "prune";

const ALL_RESOURCES: &[ResourceKind] = &[
    ResourceKind::Container,
    ResourceKind::Image,
    ResourceKind::Volume,
    ResourceKind::Network,
];

pub fn prune(app: &Backend, kind: PruneKind) -> R<()> {
    let engine = app.state().engine()?;
    let start = move || engine.prune(kind.into());
    state::run_op(
        app,
        PRUNE_PENDING_KEY.into(),
        ALL_RESOURCES,
        Op::Prune(kind),
        start,
        // `run_op` re-lists ALL once the prune is done and, since ALL includes images,
        // re-reads disk usage too: build cache has no events, so that is what updates
        // the DISK meter now.
        move |app, r: engine::PruneReport| {
            state::notice(
                app,
                Notice::Pruned {
                    kind,
                    deleted: r.deleted,
                    reclaimed: r.reclaimed,
                },
            );
        },
    )
}

/// Runs a multi-resource cleanup in dependency order (`Containers -> Networks -> Images
/// -> Volumes -> BuildCache`), sharing the `"prune"` lease with single-page prunes.
pub fn cleanup(app: &Backend, scope: CleanupScope) -> R<()> {
    if scope.is_empty() {
        return Err(invalid("Select at least one item to clean up."));
    }
    let engine = app.state().engine()?;
    let scope: engine::CleanupScope = scope.into();
    let start = move || engine.cleanup(scope);
    state::run_op(
        app,
        PRUNE_PENDING_KEY.into(),
        ALL_RESOURCES,
        Op::Cleanup,
        start,
        move |app, r: engine::CleanupReport| {
            app.send(UiEvent::CleanupFinished(r.into()));
        },
    )
}

pub fn pull(app: &Backend, reference: &str) -> R<()> {
    let reference = reference.trim().to_owned();
    if reference.is_empty() || reference.chars().any(char::is_whitespace) {
        return Err(invalid("Enter an image reference such as nginx:1.27."));
    }
    state::pull(app, app.state().engine()?, reference)
}

/// Runs `docker compose` for a project known from its containers' labels.
pub fn compose(app: &Backend, project: &str, command: ComposeCommand) -> R<()> {
    let labels = app
        .state()
        .compose_labels(project)
        .ok_or_else(|| invalid(format!("Unknown compose project {project}")))?;
    state::compose(app, labels, command.into())
}

/// Cancels an active `docker compose` run for `project`.
pub fn cancel_compose(app: &Backend, project: &str) -> R<()> {
    state::cancel_compose(app, project)
}

/// Dismisses a finished `docker compose` run output card for `project`.
pub fn dismiss_compose(app: &Backend, project: &str) {
    state::dismiss_compose(app, project);
}

pub fn subscribe_logs(app: &Backend, id: String) -> R<u32> {
    // Fetch as much history as the view will keep, bounded to keep the first batch sane.
    let tail = app.config().log_lines().min(10_000) as usize;
    let engine = app.state().engine()?;
    let ui = app.clone();
    Ok(app.streams().logs(engine, id, tail, move |sub| {
        Box::new(move |batch| ui.send(UiEvent::Logs(sub, batch)))
    }))
}

pub fn subscribe_stats(app: &Backend, id: String) -> R<u32> {
    let engine = app.state().engine()?;
    let ui = app.clone();
    Ok(app.streams().stats(engine, id, move |sub| {
        Box::new(move |ev| ui.send(UiEvent::Stats(sub, ev)))
    }))
}

pub async fn exec_start(
    app: Backend,
    id: String,
    shell: ExecShell,
    cols: u16,
    rows: u16,
) -> R<u32> {
    let spec = run_spec::parse_exec(&shell)?;
    let engine = app.state().engine()?;
    let ui = app.clone();
    app.streams()
        .exec(
            engine,
            id,
            spec,
            TtySize {
                cols: cols.max(2),
                rows: rows.max(1),
            },
            move |sub| Box::new(move |out| ui.send(UiEvent::Exec(sub, out))),
        )
        .await
}

/// Writes keys (or the terminal's replies) to an exec session.
pub fn exec_input(app: &Backend, session: u32, data: Vec<u8>) -> R<()> {
    app.streams().exec_send(session, ExecInput::Bytes(data))
}

pub fn exec_resize(app: &Backend, session: u32, cols: u16, rows: u16) -> R<()> {
    app.streams().exec_send(
        session,
        ExecInput::Resize(TtySize {
            cols: cols.max(2),
            rows: rows.max(1),
        }),
    )
}

/// Releases flow-control credits for a `logs` or `exec` stream batch.
pub fn ack_stream(app: &Backend, id: u32, credits: u32) {
    app.streams().ack(id, credits);
}

/// Ends a log/stats subscription or an exec session.
pub fn unsubscribe(app: &Backend, id: u32) {
    app.streams().unsubscribe(id);
}

pub fn get_config(app: &Backend) -> Config {
    app.config().get()
}

/// Saves UI settings (the context is changed with `use_context`, not here).
pub async fn set_config(app: Backend, value: Config) -> R<Config> {
    let saved = app
        .config()
        .update_with_save(
            move |c| {
                let context = c.context.clone();
                *c = Config { context, ..value }.normalized();
            },
            |c| c.save(),
        )
        .await
        .map_err(|e| invalid(format!("Could not save settings: {e}")))?;
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{
        UpdateOutcome, copy_name, each, env_key, resolve_target_for_context, update_outcome,
        valid_name,
    };
    use crate::config::{Config, ConfigStore};

    #[test]
    fn a_left_out_variable_is_named_without_its_value() {
        assert_eq!(env_key("CERT=-----BEGIN\nsecret"), "CERT");
        assert_eq!(env_key(" PAD=x "), "PAD");
        assert_eq!(env_key("BARE"), "BARE");
    }

    #[test]
    fn a_copy_gets_a_free_name() {
        let taken = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(copy_name("web", &taken(&["web"])), "web-copy");
        assert_eq!(
            copy_name("web", &taken(&["web", "web-copy", "web-copy-2"])),
            "web-copy-3"
        );
        assert_eq!(copy_name("", &taken(&["web"])), "");
        assert!(valid_name(&copy_name("shop-web-1", &[])).is_ok());
    }

    #[test]
    fn resource_names_are_validated() {
        assert_eq!(valid_name(" data_1 ").unwrap(), "data_1");
        assert!(valid_name("").is_err());
        assert!(valid_name("-x").is_err());
        assert!(valid_name("a b").is_err());
        assert!(valid_name("a/b").is_err());
    }

    #[test]
    fn an_update_that_could_not_put_the_original_back_is_an_outcome_to_tell() {
        let not_restored = engine::EngineError::NotRestored {
            error: Box::new(engine::EngineError::Local("boom".into())),
            kept_as: "web-replaced-1".into(),
            stopped: true,
        };
        assert_eq!(
            update_outcome(Err(not_restored)),
            Ok(UpdateOutcome::NotRestored {
                error: "boom".into(),
                kept_as: "web-replaced-1".into(),
                stopped: true,
            })
        );
        let failed = engine::EngineError::Local("boom".into());
        assert_eq!(update_outcome(Err(failed.clone())), Err(failed));
        assert_eq!(
            update_outcome(Ok(engine::UpdateOutcome::UpToDate)),
            Ok(UpdateOutcome::UpToDate)
        );
    }

    /// A failing item does not stop the rest, and the first error is returned.
    #[test]
    fn each_runs_every_item_and_returns_the_first_error() {
        let mut ran = Vec::new();
        let r = each(vec![1, 2, 3], |i| {
            ran.push(i);
            if i >= 2 {
                Err(super::invalid(format!("no {i}")))
            } else {
                Ok(())
            }
        });
        assert_eq!(ran, [1, 2, 3]);
        assert_eq!(r.unwrap_err().message, "no 2");
    }

    #[tokio::test]
    async fn use_context_resolves_directly_and_config_store_releases_mem_lock_during_disk_save() {
        // A-003, A-006 / R-03 + P-011
        let lookups = Arc::new(AtomicUsize::new(0));
        let find = |n: &str| {
            lookups.fetch_add(1, Ordering::SeqCst);
            match n {
                "prod" => Some(engine::ContextEntry {
                    name: "prod".into(),
                    host: "tcp://10.0.0.2:2376".into(),
                    current: false,
                    supported: true,
                    tls: None,
                }),
                "sshbox" => Some(engine::ContextEntry {
                    name: "sshbox".into(),
                    host: "ssh://ops@10.0.0.3".into(),
                    current: false,
                    supported: false,
                    tls: None,
                }),
                _ => None,
            }
        };
        let cli_default = || engine::ConnectTarget {
            host: "unix:///var/run/docker.sock".into(),
            source: "DOCKER_HOST".into(),
            context: None,
            tls: None,
        };

        assert!(resolve_target_for_context(find, cli_default, Some("sshbox")).is_err());
        assert_eq!(lookups.load(Ordering::SeqCst), 1);

        let target = resolve_target_for_context(find, cli_default, Some("prod")).unwrap();
        assert_eq!(target.context.as_deref(), Some("prod"));
        assert_eq!(lookups.load(Ordering::SeqCst), 2);

        let reset_target = resolve_target_for_context(find, cli_default, None).unwrap();
        assert_eq!(reset_target.context, None);
        assert_eq!(reset_target.source, "DOCKER_HOST");

        // Verify ConfigStore::get does not block while disk save is in flight on spawn_blocking.
        let store = Arc::new(ConfigStore::new(Config::default()));
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let store_cl = store.clone();
        let save_fut = tokio::spawn(async move {
            store_cl
                .update_with_save(
                    |c| c.context = Some("prod".into()),
                    move |_| {
                        let _ = entered_tx.send(());
                        let _ = release_rx.recv();
                        Ok(())
                    },
                )
                .await
                .unwrap()
        });
        entered_rx.await.unwrap();
        // Memory lock is NOT held while blocking save_fn waits on disk!
        assert_eq!(store.get().context.as_deref(), Some("prod"));
        release_tx.send(()).unwrap();
        let saved = save_fut.await.unwrap();
        assert_eq!(saved.context.as_deref(), Some("prod"));
    }

    #[tokio::test]
    async fn cleanup_refuses_empty_scope_and_shares_prune_lock() {
        let target = engine::ConnectTarget {
            host: "unix:///var/run/docker.sock".into(),
            source: "test".into(),
            context: None,
            tls: None,
        };
        let (app, _rx) = crate::backend::Backend::new(
            crate::state::AppState::new(target),
            ConfigStore::new(Config::default()),
        );
        let empty = super::CleanupScope::default();
        let err = super::cleanup(&app, empty).unwrap_err();
        assert_eq!(err.kind, super::UiErrorKind::Local);

        let (hold_tx, hold_rx) = tokio::sync::oneshot::channel::<Result<(), engine::EngineError>>();
        crate::state::run_op(
            &app,
            super::PRUNE_PENDING_KEY.into(),
            &[],
            super::Op::Prune(super::PruneKind::Images),
            move || async move { hold_rx.await.unwrap() },
            |_, ()| {},
        )
        .unwrap();
        let conflict = crate::state::run_op(
            &app,
            super::PRUNE_PENDING_KEY.into(),
            &[],
            super::Op::Cleanup,
            || async { Ok(()) },
            |_, ()| {},
        )
        .unwrap_err();
        assert_eq!(conflict.kind, super::UiErrorKind::Conflict);
        let _ = hold_tx.send(Ok(()));
    }
}
