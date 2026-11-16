//! Docker Engine access for the GUI.
//!
//! This crate owns the tokio runtime that bollard/hyper need and exposes only
//! runtime-agnostic types: [`EngineTask`] (awaitable from any executor) and
//! [`EngineStream`] for streams. Dropping a stream stops the producing task.
//! No bollard type crosses this crate's boundary.

mod compose;
mod daemon;
mod error;
mod exec;
mod model;
mod ops;
mod recreate;
mod runtime;
mod target;
mod template;

pub use compose::{ComposeCommand, ComposeOutput, compose_available, run_compose};
pub use daemon::{DAEMON_READY_TIMEOUT_SECS, daemon_is_down, daemon_starter, start_daemon};
pub use error::EngineError;
pub use exec::{ExecControl, ExecInput, ExecSession, ExecSpec, TtySize};
pub use model::*;
pub use runtime::{EngineStream, EngineTask, set_runtime_handle};
pub use target::{
    ConnectTarget, ContextEntry, ROOTLESS_CONTEXT, TlsFiles, find_context, list_contexts,
    resolve_default_target,
};

use std::sync::Arc;

/// A connected engine. Cheap to clone.
#[derive(Clone)]
pub struct Engine {
    docker: bollard::Docker,
    target: Arc<ConnectTarget>,
    api_version: String,
}

impl Engine {
    /// Connects to `target`, negotiates the API version and reads the version info.
    pub fn connect(target: ConnectTarget) -> EngineTask<Result<(Engine, EngineInfo), EngineError>> {
        runtime::spawn(async move {
            target.preflight().await?;
            let raw = match target.tls.clone() {
                Some(tls) => {
                    let host = target.host.clone();
                    tokio::task::spawn_blocking(move || {
                        bollard::Docker::connect_with_ssl(
                            &host,
                            &tls.key,
                            &tls.cert,
                            &tls.ca,
                            120,
                            bollard::API_DEFAULT_VERSION,
                        )
                    })
                    .await
                    .map_err(|e| EngineError::Local(format!("TLS init task failed: {e}")))?
                }
                None => bollard::Docker::connect_with_host(&target.host),
            };
            let docker = raw
                .map_err(|e| EngineError::classify(e, &target))?
                .negotiate_version()
                .await
                .map_err(|e| EngineError::classify(e, &target))?;
            let v = docker
                .version()
                .await
                .map_err(|e| EngineError::classify(e, &target))?;
            let api_version = v.api_version.unwrap_or_default();
            let info = EngineInfo {
                version: v.version.unwrap_or_default(),
                api_version: api_version.clone(),
                os: v.os.unwrap_or_default(),
                arch: v.arch.unwrap_or_default(),
                host: target.host.clone(),
            };
            tracing::info!(
                host = %target.host,
                source = %target.source,
                version = %info.version,
                "connected to Docker Engine"
            );
            Ok((
                Engine {
                    docker,
                    target: Arc::new(target),
                    api_version,
                },
                info,
            ))
        })
    }

    pub fn target(&self) -> &ConnectTarget {
        &self.target
    }

    pub fn api_version(&self) -> &str {
        &self.api_version
    }

    fn err(&self, e: bollard::errors::Error) -> EngineError {
        let classified = EngineError::classify(e, &self.target);
        tracing::debug!(host = %self.target.host, error = %classified, "engine request failed");
        classified
    }
}
