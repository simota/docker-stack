//! The handle the UI holds on the backend: shared state, settings, live streams, and
//! the channel that carries everything the backend pushes to the UI.

use std::future::Future;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};

use noa_gpui_platform::Waker;

use crate::config::{Config, ConfigStore};
use crate::dto::{
    CleanupReport, ContainerExit, ContainerUpdate, ContextEntry, ExecOutput, LogBatch, Notice,
    RunTemplate, Snapshot, StatePatch, StatsEvent, UiError, UpdateCheck,
};
use crate::state::AppState;
use crate::streams::Streams;

/// Something the backend pushes to the UI.
#[derive(Debug)]
pub enum UiEvent {
    State(Box<StatePatch>),
    Notice(Notice),
    Logs(u32, LogBatch),
    Stats(u32, StatsEvent),
    Exec(u32, ExecOutput),
    /// An exec session for `container` started (or failed to).
    ExecStarted {
        container: String,
        result: Result<u32, UiError>,
    },
    Inspect {
        id: String,
        result: Result<String, UiError>,
    },
    Contexts(Result<Vec<ContextEntry>, UiError>),
    /// A Run form filled in from a container (Duplicate), or why it could not be.
    RunTemplate(Result<RunTemplate, UiError>),
    /// A container failed on its own (see [`ContainerExit`]).
    ContainerExited(ContainerExit),
    /// An Update finished (see [`crate::commands::update_container`]).
    ContainerUpdated(ContainerUpdate),
    /// A check for newer images finished (see [`crate::commands::check_updates`]).
    UpdateChecks(Result<Vec<UpdateCheck>, UiError>),
    /// A cross-resource cleanup finished (see [`crate::commands::cleanup`]).
    CleanupFinished(CleanupReport),
    Config(Config),
}

struct Inner {
    state: AppState,
    config: ConfigStore,
    streams: Streams,
    tx: Mutex<Sender<UiEvent>>,
    /// Wakes the UI's event loop after each event (set once the app exists).
    waker: OnceLock<Waker>,
}

#[derive(Clone)]
pub struct Backend(Arc<Inner>);

impl Backend {
    pub fn new(state: AppState, config: ConfigStore) -> (Self, Receiver<UiEvent>) {
        let (tx, rx) = channel();
        let inner = Inner {
            state,
            config,
            streams: Streams::default(),
            tx: Mutex::new(tx),
            waker: OnceLock::new(),
        };
        (Self(Arc::new(inner)), rx)
    }

    pub fn state(&self) -> &AppState {
        &self.0.state
    }

    pub fn config(&self) -> &ConfigStore {
        &self.0.config
    }

    pub fn streams(&self) -> &Streams {
        &self.0.streams
    }

    pub fn snapshot(&self) -> Snapshot {
        self.0.state.snapshot()
    }

    /// Events sent from now on wake the UI with `waker`.
    pub fn set_waker(&self, waker: Waker) {
        let _ = self.0.waker.set(waker);
    }

    /// Returns false once the UI is gone.
    pub fn send(&self, event: UiEvent) -> bool {
        let sent = self
            .0
            .tx
            .lock()
            .expect("ui channel lock")
            .send(event)
            .is_ok();
        if sent && let Some(waker) = self.0.waker.get() {
            waker.wake();
        }
        sent
    }

    /// Runs `fut` on the backend runtime (the main thread entered it at launch).
    pub fn spawn<F>(&self, fut: F) -> tokio::task::JoinHandle<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        tokio::spawn(fut)
    }
}
