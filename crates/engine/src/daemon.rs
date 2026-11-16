//! Starting a local Docker daemon that is not running (automatically at app
//! launch, or when the user asks).
//!
//! Only local unix-socket targets whose socket location identifies a known
//! daemon are started; anything else (tcp, unknown sockets such as colima)
//! is left alone and the normal reconnect loop reports it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

use crate::runtime::{self, EngineTask};
use crate::{ConnectTarget, EngineError};

/// How long the start command itself may run (pkexec waits for a password).
const START_TIMEOUT: Duration = Duration::from_secs(120);
/// How long to wait for the socket to accept connections after starting.
const READY_TIMEOUT: Duration = Duration::from_secs(90);
const READY_POLL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Os {
    Linux,
    MacOs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StartCommand {
    /// Shown to the user ("Docker Desktop", "docker.service").
    pub label: &'static str,
    pub program: &'static str,
    pub args: &'static [&'static str],
}

pub(crate) fn start_command_for(
    os: Os,
    socket: &Path,
    home: Option<&Path>,
    xdg_runtime_dir: Option<&Path>,
) -> Option<StartCommand> {
    let under_home = |rel: &str| home.is_some_and(|h| socket == h.join(rel));
    let system =
        socket == Path::new("/var/run/docker.sock") || socket == Path::new("/run/docker.sock");
    match os {
        Os::Linux if system => Some(StartCommand {
            label: "docker.service",
            program: "pkexec",
            args: &["systemctl", "start", "docker.service"],
        }),
        Os::Linux if xdg_runtime_dir.is_some_and(|d| socket == d.join("docker.sock")) => {
            Some(StartCommand {
                label: "docker.service (rootless)",
                program: "systemctl",
                args: &["--user", "start", "docker.service"],
            })
        }
        Os::Linux if under_home(".docker/desktop/docker.sock") => Some(StartCommand {
            label: "Docker Desktop",
            program: "systemctl",
            args: &["--user", "start", "docker-desktop.service"],
        }),
        // On macOS /var/run/docker.sock is Docker Desktop's symlink.
        Os::MacOs if system || under_home(".docker/run/docker.sock") => Some(StartCommand {
            label: "Docker Desktop",
            program: "/usr/bin/open",
            args: &["-a", "Docker"],
        }),
        _ => None,
    }
}

fn current_os() -> Option<Os> {
    if cfg!(target_os = "linux") {
        Some(Os::Linux)
    } else if cfg!(target_os = "macos") {
        Some(Os::MacOs)
    } else {
        None
    }
}

/// True when nothing listens on the socket: the file is missing or refuses connections.
async fn daemon_down(socket: &Path) -> bool {
    match tokio::time::timeout(
        crate::target::PREFLIGHT_TIMEOUT,
        tokio::net::UnixStream::connect(socket),
    )
    .await
    {
        Ok(Err(e)) => matches!(
            e.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        ),
        _ => false,
    }
}

async fn run_start(cmd: &StartCommand) -> Result<(), EngineError> {
    let fail = |why: String| EngineError::Local(format!("starting {} failed: {why}", cmd.label));
    let out = tokio::time::timeout(
        START_TIMEOUT,
        Command::new(cmd.program)
            .args(cmd.args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| fail("timed out".into()))?
    .map_err(|e| fail(format!("{}: {e}", cmd.program)))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let detail = stderr.trim();
    Err(fail(if detail.is_empty() {
        out.status.to_string()
    } else {
        detail.to_owned()
    }))
}

async fn wait_ready(socket: &Path, label: &str) -> Result<(), EngineError> {
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        if tokio::net::UnixStream::connect(socket).await.is_ok() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(EngineError::Local(format!(
                "started {label}, but {} did not accept connections within {}s",
                socket.display(),
                READY_TIMEOUT.as_secs()
            )));
        }
        tokio::time::sleep(READY_POLL).await;
    }
}

/// Seconds `start_daemon` waits for the socket after the start command returns.
pub const DAEMON_READY_TIMEOUT_SECS: u64 = READY_TIMEOUT.as_secs();

fn start_command(target: &ConnectTarget) -> Option<(PathBuf, StartCommand)> {
    let os = current_os()?;
    let socket = target.socket_path()?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let cmd = start_command_for(os, &socket, home.as_deref(), xdg.as_deref())?;
    Some((socket, cmd))
}

/// The daemon this app knows how to start for `target` ("Docker Desktop", ...), if any.
pub fn daemon_starter(target: &ConnectTarget) -> Option<&'static str> {
    start_command(target).map(|(_, c)| c.label)
}

/// True when `target` is a local socket nothing listens on.
pub fn daemon_is_down(target: ConnectTarget) -> EngineTask<bool> {
    runtime::spawn(async move {
        match target.socket_path() {
            Some(socket) => daemon_down(&socket).await,
            None => false,
        }
    })
}

/// Starts the daemon behind `target` and waits until its socket accepts connections.
/// Returns the started daemon's label.
pub fn start_daemon(target: ConnectTarget) -> EngineTask<Result<&'static str, EngineError>> {
    runtime::spawn(async move {
        let Some((socket, cmd)) = start_command(&target) else {
            return Err(EngineError::Local(format!(
                "Docker Stack does not know how to start the daemon behind {}.",
                target.host
            )));
        };
        tracing::info!(
            daemon = cmd.label,
            program = cmd.program,
            "starting docker daemon"
        );
        run_start(&cmd).await?;
        wait_ready(&socket, cmd.label).await?;
        tracing::info!(daemon = cmd.label, "docker daemon started");
        Ok(cmd.label)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(os: Os, socket: &str) -> Option<&'static str> {
        start_command_for(
            os,
            Path::new(socket),
            Some(Path::new("/home/u")),
            Some(Path::new("/run/user/1000")),
        )
        .map(|c| c.program)
    }

    #[test]
    fn picks_start_command_from_socket_location() {
        assert_eq!(cmd(Os::Linux, "/var/run/docker.sock"), Some("pkexec"));
        assert_eq!(cmd(Os::Linux, "/run/docker.sock"), Some("pkexec"));
        assert_eq!(
            cmd(Os::Linux, "/run/user/1000/docker.sock"),
            Some("systemctl")
        );
        let desktop = start_command_for(
            Os::Linux,
            Path::new("/home/u/.docker/desktop/docker.sock"),
            Some(Path::new("/home/u")),
            None,
        )
        .unwrap();
        assert_eq!(desktop.args, ["--user", "start", "docker-desktop.service"]);
        assert_eq!(
            cmd(Os::MacOs, "/home/u/.docker/run/docker.sock"),
            Some("/usr/bin/open")
        );
        assert_eq!(
            cmd(Os::MacOs, "/var/run/docker.sock"),
            Some("/usr/bin/open")
        );
    }

    #[test]
    fn leaves_unknown_daemons_alone() {
        assert_eq!(cmd(Os::Linux, "/home/u/.colima/default/docker.sock"), None);
        assert_eq!(cmd(Os::MacOs, "/home/u/.colima/default/docker.sock"), None);
        assert_eq!(cmd(Os::MacOs, "/run/user/1000/docker.sock"), None);
        // Desktop's Linux socket path means nothing on macOS, and vice versa.
        assert_eq!(cmd(Os::MacOs, "/home/u/.docker/desktop/docker.sock"), None);
        assert_eq!(cmd(Os::Linux, "/home/u/.docker/run/docker.sock"), None);
    }

    #[test]
    fn tcp_target_is_never_started() {
        let t = ConnectTarget {
            host: "tcp://127.0.0.1:1".into(),
            source: String::new(),
            context: None,
            tls: None,
        };
        assert_eq!(daemon_starter(&t), None);
        assert!(!futures::executor::block_on(daemon_is_down(t.clone())));
        assert!(futures::executor::block_on(start_daemon(t)).is_err());
    }

    #[test]
    fn missing_socket_counts_as_down() {
        let p = std::env::temp_dir().join(format!("ds-no-daemon-{}.sock", std::process::id()));
        assert!(futures::executor::block_on(runtime::spawn(async move {
            daemon_down(&p).await
        })));
    }
}
