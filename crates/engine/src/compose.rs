//! Compose operations shell out to `docker compose` (v2). Reimplementing the
//! compose spec (profiles, depends_on, build, interpolation) is not worth it.

use std::process::Stdio;
use std::time::Duration;

use futures::SinkExt;
use futures::channel::mpsc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::task::AbortHandle;

use crate::runtime::{EngineStream, EngineTask, handle, spawn};
use crate::{ComposeLabels, ConnectTarget};

const COMPOSE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const DEFAULT_COMPOSE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
pub(crate) const MAX_COMPOSE_LINE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeCommand {
    Up,
    Down,
    Restart,
    Stop,
    Start,
    Pull,
}

impl ComposeCommand {
    pub fn args(self) -> &'static [&'static str] {
        match self {
            ComposeCommand::Up => &["up", "-d"],
            ComposeCommand::Down => &["down"],
            ComposeCommand::Restart => &["restart"],
            ComposeCommand::Stop => &["stop"],
            ComposeCommand::Start => &["start"],
            ComposeCommand::Pull => &["pull"],
        }
    }
    pub fn label(self) -> &'static str {
        self.args()[0]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeOutput {
    Line(String),
    /// The process ended; `Ok(())` on exit status 0.
    Exit(Result<(), String>),
}

/// Whether `docker compose` (the v2 plugin) can be run.
pub fn compose_available() -> EngineTask<bool> {
    spawn(async {
        let mut cmd = Command::new("docker");
        cmd.args(["compose", "version"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        matches!(
            tokio::time::timeout(COMPOSE_PROBE_TIMEOUT, cmd.status()).await,
            Ok(Ok(s)) if s.success()
        )
    })
}

/// Builds the argv for a compose command against one project.
pub(crate) fn compose_argv(labels: &ComposeLabels, cmd: ComposeCommand) -> Vec<String> {
    let mut argv = vec![
        "compose".to_string(),
        "-p".to_string(),
        labels.project.clone(),
    ];
    if let Some(dir) = &labels.working_dir {
        argv.push("--project-directory".into());
        argv.push(dir.clone());
    }
    for f in &labels.config_files {
        argv.push("-f".into());
        argv.push(f.clone());
    }
    argv.extend(cmd.args().iter().map(|s| s.to_string()));
    argv
}

pub(crate) fn compose_env(target: &ConnectTarget) -> Vec<(&'static str, Option<String>)> {
    if let Some(ctx) = target
        .context
        .as_deref()
        .filter(|c| *c != crate::ROOTLESS_CONTEXT && target.tls.is_some())
    {
        return vec![
            ("DOCKER_HOST", None),
            ("DOCKER_CONTEXT", Some(ctx.to_owned())),
            ("DOCKER_TLS_VERIFY", None),
            ("DOCKER_CERT_PATH", None),
        ];
    }
    if let Some(tls) = &target.tls {
        return vec![
            ("DOCKER_HOST", Some(target.host.clone())),
            ("DOCKER_CONTEXT", None),
            ("DOCKER_TLS_VERIFY", Some("1".into())),
            (
                "DOCKER_CERT_PATH",
                tls.ca.parent().map(|p| p.display().to_string()),
            ),
        ];
    }
    vec![
        ("DOCKER_HOST", Some(target.host.clone())),
        ("DOCKER_CONTEXT", None),
        ("DOCKER_TLS_VERIFY", None),
        ("DOCKER_CERT_PATH", None),
    ]
}

struct ChildTaskGuard(Vec<AbortHandle>);

impl Drop for ChildTaskGuard {
    fn drop(&mut self) {
        for h in self.0.drain(..) {
            h.abort();
        }
    }
}

/// Runs `docker compose` for `labels.project` against `target`, streaming
/// merged stdout/stderr lines and finally an [`ComposeOutput::Exit`].
pub fn run_compose(
    target: &ConnectTarget,
    labels: &ComposeLabels,
    cmd: ComposeCommand,
) -> EngineStream<ComposeOutput> {
    run_compose_with_timeout(target, labels, cmd, DEFAULT_COMPOSE_TIMEOUT)
}

/// Like [`run_compose`], with an explicit wall-clock deadline after which the
/// child process is killed and [`ComposeOutput::Exit`] reports a timeout error.
pub(crate) fn run_compose_with_timeout(
    target: &ConnectTarget,
    labels: &ComposeLabels,
    cmd: ComposeCommand,
    timeout: Duration,
) -> EngineStream<ComposeOutput> {
    let argv = compose_argv(labels, cmd);
    let desc = format!("docker compose {}", argv[3..].join(" "));
    let mut command = Command::new("docker");
    command.args(&argv);
    for (k, v) in compose_env(target) {
        match v {
            Some(val) => {
                command.env(k, val);
            }
            None => {
                command.env_remove(k);
            }
        }
    }
    spawn_logged_command(command, labels.working_dir.clone(), desc, timeout)
}

pub(crate) fn spawn_logged_command(
    mut command: Command,
    cwd: Option<String>,
    desc: String,
    timeout: Duration,
) -> EngineStream<ComposeOutput> {
    let (mut tx, rx) = mpsc::channel(256);
    let task = handle().spawn(async move {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = cwd {
            let check_dir = dir.clone();
            let exists =
                tokio::task::spawn_blocking(move || std::path::Path::new(&check_dir).is_dir())
                    .await
                    .unwrap_or(false);
            if exists {
                command.current_dir(dir);
            }
        }
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "failed to spawn {desc}");
                let _ = tx
                    .send(ComposeOutput::Exit(Err(format!(
                        "failed to run {desc}: {e}"
                    ))))
                    .await;
                return;
            }
        };
        let (ltx, mut lrx) = mpsc::channel::<String>(256);
        let out_task = handle().spawn(forward_lines(
            BufReader::new(child.stdout.take().expect("piped")),
            ltx.clone(),
        ));
        let err_task = handle().spawn(forward_lines(
            BufReader::new(child.stderr.take().expect("piped")),
            ltx,
        ));
        let readers = ChildTaskGuard(vec![out_task.abort_handle(), err_task.abort_handle()]);
        use futures::StreamExt;
        let outcome = tokio::time::timeout(timeout, async {
            while let Some(line) = lrx.next().await {
                if tx.send(ComposeOutput::Line(line)).await.is_err() {
                    return None;
                }
            }
            Some(child.wait().await)
        })
        .await;
        drop(readers);
        let result = match outcome {
            Ok(None) => return,
            Ok(Some(Ok(s))) if s.success() => Ok(()),
            Ok(Some(Ok(s))) => Err(format!("{desc} exited with {s}")),
            Ok(Some(Err(e))) => Err(e.to_string()),
            Err(_) => {
                let _ = child.kill().await;
                Err(format!(
                    "{desc} timed out after {}s",
                    timeout.as_secs().max(1)
                ))
            }
        };
        let _ = tx.send(ComposeOutput::Exit(result)).await;
    });
    EngineStream::aborted(rx, task.abort_handle())
}

async fn forward_lines<R: tokio::io::AsyncBufRead + Unpin>(
    mut reader: R,
    mut tx: mpsc::Sender<String>,
) {
    let mut buf: Vec<u8> = Vec::new();
    let mut truncated = false;
    loop {
        let chunk = match reader.fill_buf().await {
            Ok([]) | Err(_) => break,
            Ok(c) => c,
        };
        if let Some(nl) = chunk.iter().position(|&b| b == b'\n') {
            let has_cr = nl > 0 && chunk[nl - 1] == b'\r';
            let line_part = if has_cr {
                &chunk[..nl - 1]
            } else {
                &chunk[..nl]
            };
            if !truncated {
                let room = MAX_COMPOSE_LINE_BYTES.saturating_sub(buf.len());
                if line_part.len() <= room {
                    buf.extend_from_slice(line_part);
                } else {
                    buf.extend_from_slice(&line_part[..room]);
                    truncated = true;
                }
            }
            reader.consume(nl + 1);
            if nl == 0 && !truncated && buf.last() == Some(&b'\r') {
                buf.pop();
            }
            let mut text = String::from_utf8_lossy(&buf).into_owned();
            if truncated {
                text.push('…');
            }
            buf.clear();
            if buf.capacity() > MAX_COMPOSE_LINE_BYTES {
                buf.shrink_to(MAX_COMPOSE_LINE_BYTES);
            }
            truncated = false;
            if tx.send(text).await.is_err() {
                return;
            }
        } else {
            let len = chunk.len();
            if !truncated {
                let room = MAX_COMPOSE_LINE_BYTES.saturating_sub(buf.len());
                if len <= room {
                    buf.extend_from_slice(chunk);
                } else {
                    buf.extend_from_slice(&chunk[..room]);
                    truncated = true;
                }
            }
            reader.consume(len);
        }
    }
    if !buf.is_empty() || truncated {
        let mut text = String::from_utf8_lossy(&buf).into_owned();
        if truncated {
            text.push('…');
        }
        let _ = tx.send(text).await;
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::TlsFiles;

    #[test]
    fn argv_includes_project_dir_and_files() {
        let labels = ComposeLabels {
            project: "myapp".into(),
            service: "web".into(),
            working_dir: Some("/src/myapp".into()),
            config_files: vec!["/src/myapp/compose.yaml".into()],
        };
        assert_eq!(
            compose_argv(&labels, ComposeCommand::Up),
            [
                "compose",
                "-p",
                "myapp",
                "--project-directory",
                "/src/myapp",
                "-f",
                "/src/myapp/compose.yaml",
                "up",
                "-d"
            ]
        );
        let bare = ComposeLabels {
            project: "p".into(),
            ..Default::default()
        };
        assert_eq!(
            compose_argv(&bare, ComposeCommand::Down),
            ["compose", "-p", "p", "down"]
        );
    }

    #[test]
    fn compose_env_propagates_tls_context_and_cert_dir() {
        // A-003 / R-03
        let tls = TlsFiles {
            ca: PathBuf::from("/certs/docker/ca.pem"),
            cert: PathBuf::from("/certs/docker/cert.pem"),
            key: PathBuf::from("/certs/docker/key.pem"),
        };
        let ctx_target = ConnectTarget {
            host: "tcp://10.0.0.2:2376".into(),
            source: "context: prod".into(),
            context: Some("prod".into()),
            tls: Some(tls.clone()),
        };
        let env = compose_env(&ctx_target);
        assert!(env.contains(&("DOCKER_HOST", None)));
        assert!(env.contains(&("DOCKER_CONTEXT", Some("prod".into()))));

        let host_tls_target = ConnectTarget {
            host: "tcp://10.0.0.2:2376".into(),
            source: "DOCKER_HOST".into(),
            context: None,
            tls: Some(tls),
        };
        let env = compose_env(&host_tls_target);
        assert!(env.contains(&("DOCKER_HOST", Some("tcp://10.0.0.2:2376".into()))));
        assert!(env.contains(&("DOCKER_TLS_VERIFY", Some("1".into()))));
        assert!(env.contains(&("DOCKER_CERT_PATH", Some("/certs/docker".into()))));
    }

    #[test]
    fn forward_lines_caps_unterminated_single_line_bytes() {
        // P-009: newline-free streams are truncated at MAX_COMPOSE_LINE_BYTES instead of growing unboundedly.
        use futures::StreamExt;
        let mut raw = Vec::new();
        raw.extend_from_slice(b"first\r\n");
        raw.extend(std::iter::repeat_n(b'x', MAX_COMPOSE_LINE_BYTES * 5));
        raw.push(b'\n');
        raw.extend_from_slice(b"after\n");

        let lines: Vec<String> = futures::executor::block_on(spawn(async move {
            let (tx, rx) = mpsc::channel(16);
            // Small BufReader capacity exercises multi-chunk reads and CRLF across boundaries.
            forward_lines(BufReader::with_capacity(512, &raw[..]), tx).await;
            rx.collect().await
        }));
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "first");
        assert_eq!(lines[1].len(), MAX_COMPOSE_LINE_BYTES + '…'.len_utf8());
        assert!(lines[1].ends_with('…'));
        assert_eq!(lines[2], "after");
    }

    #[test]
    fn spawn_logged_command_kills_hung_process_on_timeout() {
        // P-010: hung child processes are killed and report a timeout exit.
        use futures::StreamExt;

        let mut ok_cmd = Command::new("sh");
        ok_cmd.args(["-c", "printf 'hello\\n'"]);
        let ok_out: Vec<ComposeOutput> = futures::executor::block_on(
            spawn_logged_command(ok_cmd, None, "test-ok".into(), Duration::from_secs(2)).collect(),
        );
        assert_eq!(
            ok_out,
            vec![
                ComposeOutput::Line("hello".into()),
                ComposeOutput::Exit(Ok(())),
            ]
        );

        let mut hung_cmd = Command::new("sh");
        hung_cmd.args(["-c", "printf 'started\\n'; sleep 30"]);
        let hung_out: Vec<ComposeOutput> = futures::executor::block_on(
            spawn_logged_command(
                hung_cmd,
                None,
                "docker compose up -d".into(),
                Duration::from_millis(80),
            )
            .collect(),
        );
        assert_eq!(
            hung_out.first(),
            Some(&ComposeOutput::Line("started".into()))
        );
        assert!(
            matches!(hung_out.last(), Some(ComposeOutput::Exit(Err(msg))) if msg.contains("timed out")),
            "{hung_out:?}"
        );
    }
}
