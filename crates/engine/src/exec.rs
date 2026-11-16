//! Interactive `exec` sessions with a TTY.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use bollard::exec::{StartExecOptions, StartExecResults};
use bollard::models::ExecConfig;
use bollard::query_parameters::ResizeExecOptions;
use futures::channel::{mpsc, oneshot};
use futures::{SinkExt, Stream, StreamExt};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::runtime::{EngineStream, EngineTask, handle, spawn};
use crate::{Engine, EngineError};

pub(crate) const EXEC_STDIN_CAPACITY: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TtySize {
    pub cols: u16,
    pub rows: u16,
}

/// What an exec runs, and as whom and where; `None` keeps the container's own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecSpec {
    pub cmd: Vec<String>,
    pub user: Option<String>,
    pub working_dir: Option<String>,
}

fn exec_config(spec: ExecSpec, size: TtySize) -> ExecConfig {
    ExecConfig {
        attach_stdin: Some(true),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        tty: Some(true),
        console_size: Some(vec![size.rows as usize, size.cols as usize]),
        env: Some(vec!["TERM=xterm-256color".into()]),
        cmd: Some(spec.cmd),
        user: spec.user,
        working_dir: spec.working_dir,
        ..Default::default()
    }
}

pub enum ExecInput {
    Bytes(Vec<u8>),
    Resize(TtySize),
}

/// Bounded handle for driving an [`ExecSession`]: stdin writes go into a bounded
/// queue while TTY resizes coalesce in a latest-wins watch slot.
#[derive(Clone)]
pub struct ExecControl {
    stdin_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    resize_tx: Arc<tokio::sync::watch::Sender<TtySize>>,
}

impl ExecControl {
    pub fn send(&self, input: ExecInput) -> Result<(), EngineError> {
        match input {
            ExecInput::Bytes(bytes) => match self.stdin_tx.try_send(bytes) {
                Ok(()) => Ok(()),
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    Err(EngineError::Local("exec input buffer is full".into()))
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    Err(EngineError::Local("exec session closed".into()))
                }
            },
            ExecInput::Resize(size) => {
                if self.resize_tx.is_closed() {
                    return Err(EngineError::Local("exec session closed".into()));
                }
                self.resize_tx.send_if_modified(|cur| {
                    if *cur == size {
                        false
                    } else {
                        *cur = size;
                        true
                    }
                });
                Ok(())
            }
        }
    }
}

/// A running exec. Dropping `input` closes stdin; dropping `output` terminates the session.
pub struct ExecSession {
    pub input: ExecControl,
    pub output: EngineStream<Vec<u8>>,
}

fn spawn_exec_workers<Out, In, R, RFut>(
    initial_size: TtySize,
    mut output: Out,
    mut input: In,
    mut on_resize: R,
) -> ExecSession
where
    Out: Stream<Item = Result<Vec<u8>, String>> + Send + Unpin + 'static,
    In: AsyncWrite + Send + Unpin + 'static,
    R: FnMut(TtySize) -> RFut + Send + 'static,
    RFut: Future<Output = ()> + Send + 'static,
{
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(256);
    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(EXEC_STDIN_CAPACITY);
    let (resize_tx, mut resize_rx) = tokio::sync::watch::channel(initial_size);
    let (drop_tx, drop_rx) = oneshot::channel::<()>();

    let mut out_task = handle().spawn(async move {
        let mut out_tx = out_tx;
        let mut forwarding = true;
        while let Some(item) = output.next().await {
            match item {
                Ok(bytes) => {
                    if forwarding && out_tx.send(bytes).await.is_err() {
                        forwarding = false;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "exec output stream failed");
                    break;
                }
            }
        }
    });

    let resize_task = handle().spawn(async move {
        while resize_rx.changed().await.is_ok() {
            let s = *resize_rx.borrow_and_update();
            on_resize(s).await;
        }
    });

    handle().spawn(async move {
        let mut drop_rx = std::pin::pin!(drop_rx);
        let cancelled = loop {
            tokio::select! {
                _ = &mut drop_rx => break true,
                msg = stdin_rx.recv() => match msg {
                    Some(b) => {
                        if input.write_all(&b).await.is_err() {
                            break false;
                        }
                        let _ = input.flush().await;
                    }
                    None => break false,
                }
            }
        };
        resize_task.abort();
        if cancelled && !out_task.is_finished() {
            let _ = input.write_all(b"\x03\x15exit\n\x04").await;
            let _ = input.flush().await;
            let _ = tokio::time::timeout(Duration::from_millis(250), &mut out_task).await;
        }
        let _ = input.shutdown().await;
        out_task.abort();
    });

    ExecSession {
        input: ExecControl {
            stdin_tx,
            resize_tx: Arc::new(resize_tx),
        },
        output: EngineStream::signaled(out_rx, drop_tx),
    }
}

impl Engine {
    pub fn exec(
        &self,
        id: &str,
        spec: ExecSpec,
        size: TtySize,
    ) -> EngineTask<Result<ExecSession, EngineError>> {
        let (d, this, id) = (self.docker().clone(), self.clone(), id.to_owned());
        spawn(async move {
            let created = d
                .create_exec(&id, exec_config(spec, size))
                .await
                .map_err(|e| this.err(e))?;
            let exec_id = created.id;
            let started = d
                .start_exec(
                    &exec_id,
                    Some(StartExecOptions {
                        detach: false,
                        tty: true,
                        output_capacity: None,
                    }),
                )
                .await
                .map_err(|e| this.err(e))?;
            let StartExecResults::Attached { output, input } = started else {
                return Err(EngineError::Local("exec started detached".into()));
            };

            let mapped_out = output.map(|item| match item {
                Ok(chunk) => Ok(Vec::from(chunk.into_bytes())),
                Err(e) => Err(e.to_string()),
            });

            Ok(spawn_exec_workers(size, mapped_out, input, move |s| {
                let d = d.clone();
                let exec_id = exec_id.clone();
                async move {
                    if let Err(e) = d
                        .resize_exec(
                            &exec_id,
                            ResizeExecOptions {
                                h: s.rows as i32,
                                w: s.cols as i32,
                            },
                        )
                        .await
                    {
                        tracing::debug!(exec_id = %exec_id, error = %e, "exec resize failed");
                    }
                }
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::AsyncReadExt;

    use super::*;

    #[test]
    fn exec_config_passes_user_and_working_dir_and_keeps_the_containers_own_when_unset() {
        let size = TtySize {
            cols: 120,
            rows: 40,
        };
        let c = exec_config(
            ExecSpec {
                cmd: vec!["zsh".into()],
                user: Some("root".into()),
                working_dir: Some("/srv".into()),
            },
            size,
        );
        assert_eq!(c.cmd, Some(vec!["zsh".to_owned()]));
        assert_eq!(c.user.as_deref(), Some("root"));
        assert_eq!(c.working_dir.as_deref(), Some("/srv"));
        assert_eq!(c.console_size, Some(vec![40, 120]));
        assert_eq!(c.tty, Some(true));

        let c = exec_config(
            ExecSpec {
                cmd: vec!["sh".into()],
                ..Default::default()
            },
            size,
        );
        assert_eq!((c.user, c.working_dir), (None, None));
    }

    #[test]
    fn exec_coalesces_resize_bursts_without_blocking_stdin_and_bounds_queue() {
        // P-008: resize storms coalesce to latest-wins on a separate lane without blocking stdin,
        // and stdin queue capacity is bounded at EXEC_STDIN_CAPACITY.
        futures::executor::block_on(spawn(async {
            let (stdin_pipe_client, mut stdin_pipe_server) = tokio::io::duplex(4096);
            let out_stream = futures::stream::pending::<Result<Vec<u8>, String>>();

            let resize_calls = Arc::new(AtomicUsize::new(0));
            let recorded_sizes = Arc::new(Mutex::new(Vec::<TtySize>::new()));
            let (first_started_tx, first_started_rx) = tokio::sync::oneshot::channel::<()>();
            let (release_first_tx, release_first_rx) = tokio::sync::oneshot::channel::<()>();
            let first_started_tx = Arc::new(Mutex::new(Some(first_started_tx)));
            let release_first_rx = Arc::new(Mutex::new(Some(release_first_rx)));

            let calls_cl = resize_calls.clone();
            let sizes_cl = recorded_sizes.clone();
            let started_cl = first_started_tx.clone();
            let release_cl = release_first_rx.clone();

            let session = spawn_exec_workers(
                TtySize { cols: 80, rows: 24 },
                out_stream,
                stdin_pipe_client,
                move |sz| {
                    let calls = calls_cl.clone();
                    let sizes = sizes_cl.clone();
                    let started = started_cl.clone();
                    let release = release_cl.clone();
                    async move {
                        let idx = calls.fetch_add(1, Ordering::SeqCst);
                        sizes.lock().unwrap().push(sz);
                        if idx == 0 {
                            if let Some(tx) = started.lock().unwrap().take() {
                                let _ = tx.send(());
                            }
                            let rx = release.lock().unwrap().take();
                            if let Some(rx) = rx {
                                let _ = rx.await;
                            }
                        }
                    }
                },
            );

            // Duplicate of initial size does not fire a resize call.
            session
                .input
                .send(ExecInput::Resize(TtySize { cols: 80, rows: 24 }))
                .unwrap();

            // First distinct resize starts and blocks in flight.
            session
                .input
                .send(ExecInput::Resize(TtySize { cols: 90, rows: 25 }))
                .unwrap();
            first_started_rx.await.unwrap();
            assert_eq!(resize_calls.load(Ordering::SeqCst), 1);

            // Queue 50 rapid resize events while resize #1 is still blocked.
            for i in 1..=50u16 {
                session
                    .input
                    .send(ExecInput::Resize(TtySize {
                        cols: 90 + i,
                        rows: 25 + i,
                    }))
                    .unwrap();
            }

            // Stdin bytes must flow through immediately EVEN WHILE resize #1 is blocked!
            session
                .input
                .send(ExecInput::Bytes(b"hello-pty".to_vec()))
                .unwrap();
            let mut buf = [0u8; 9];
            tokio::time::timeout(
                Duration::from_millis(200),
                stdin_pipe_server.read_exact(&mut buf),
            )
            .await
            .expect("stdin must not block behind resize")
            .unwrap();
            assert_eq!(&buf, b"hello-pty");

            // Release resize #1; all 50 intermediate resizes coalesce into 1 final call!
            let _ = release_first_tx.send(());
            for _ in 0..40 {
                if resize_calls.load(Ordering::SeqCst) >= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert_eq!(resize_calls.load(Ordering::SeqCst), 2);
            assert_eq!(
                *recorded_sizes.lock().unwrap(),
                vec![
                    TtySize { cols: 90, rows: 25 },
                    TtySize {
                        cols: 140,
                        rows: 75
                    },
                ]
            );

            // Bounded stdin backpressure: saturate duplex pipe + channel capacity and assert cap.
            let (tiny_client, _tiny_server) = tokio::io::duplex(1);
            let capped = spawn_exec_workers(
                TtySize { cols: 80, rows: 24 },
                futures::stream::pending::<Result<Vec<u8>, String>>(),
                tiny_client,
                |_| async {},
            );
            let mut rejected = false;
            for _ in 0..(EXEC_STDIN_CAPACITY + 8) {
                if capped.input.send(ExecInput::Bytes(vec![b'a'; 64])).is_err() {
                    rejected = true;
                    break;
                }
            }
            assert!(
                rejected,
                "bounded stdin queue must reject excess chunks when consumer stalls"
            );
            drop(session);
        }));
    }
}
