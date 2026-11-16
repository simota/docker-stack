use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};

use futures::channel::{mpsc, oneshot};
use futures::{SinkExt, Stream, StreamExt};
use tokio::task::AbortHandle;

struct RuntimeCell {
    external: OnceLock<tokio::runtime::Handle>,
    fallback: OnceLock<tokio::runtime::Runtime>,
}

impl RuntimeCell {
    const fn new() -> Self {
        Self {
            external: OnceLock::new(),
            fallback: OnceLock::new(),
        }
    }

    fn set_handle(&self, handle: tokio::runtime::Handle) {
        let _ = self.external.set(handle);
    }

    fn handle(&self) -> tokio::runtime::Handle {
        if let Some(h) = self.external.get() {
            return h.clone();
        }
        self.fallback
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("engine-io")
                    .enable_all()
                    .build()
                    .expect("failed to start the tokio runtime")
            })
            .handle()
            .clone()
    }

    #[cfg(test)]
    fn has_fallback(&self) -> bool {
        self.fallback.get().is_some()
    }
}

/// Process-wide runtime selector: reuses a caller-supplied Tokio runtime when
/// installed via [`set_runtime_handle`], or lazily starts a fallback runtime
/// that lives for the whole process so it is never dropped inside async code.
static RUNTIME: RuntimeCell = RuntimeCell::new();

/// Installs an existing Tokio runtime handle for all subsequent engine work.
/// Must be called before the first engine call to avoid starting the fallback pool.
pub fn set_runtime_handle(handle: tokio::runtime::Handle) {
    RUNTIME.set_handle(handle);
}

/// A unit of work running on the engine runtime. Awaitable from any executor.
pub struct EngineTask<T>(tokio::task::JoinHandle<T>);

impl<T> Future for EngineTask<T> {
    type Output = T;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        match Pin::new(&mut self.0).poll(cx) {
            Poll::Ready(Ok(v)) => Poll::Ready(v),
            Poll::Ready(Err(e)) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
            Poll::Ready(Err(e)) => panic!("engine task cancelled: {e}"),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A stream produced on the engine runtime whose producer stops immediately on drop.
pub struct EngineStream<T> {
    rx: mpsc::Receiver<T>,
    abort: Option<AbortHandle>,
    _drop_signal: Option<oneshot::Sender<()>>,
}

impl<T> EngineStream<T> {
    pub(crate) fn aborted(rx: mpsc::Receiver<T>, abort: AbortHandle) -> Self {
        Self {
            rx,
            abort: Some(abort),
            _drop_signal: None,
        }
    }

    pub(crate) fn signaled(rx: mpsc::Receiver<T>, drop_signal: oneshot::Sender<()>) -> Self {
        Self {
            rx,
            abort: None,
            _drop_signal: Some(drop_signal),
        }
    }
}

impl<T> Stream for EngineStream<T> {
    type Item = T;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        Pin::new(&mut self.rx).poll_next(cx)
    }
}

impl<T> futures::stream::FusedStream for EngineStream<T> {
    fn is_terminated(&self) -> bool {
        self.rx.is_terminated()
    }
}

impl<T> Drop for EngineStream<T> {
    fn drop(&mut self) {
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
    }
}

pub(crate) fn spawn<F>(fut: F) -> EngineTask<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    EngineTask(handle().spawn(fut))
}

/// Forwards `stream` (mapped through `f`) into a bounded channel. Backpressure
/// slows the producer; dropping the receiver ends the task.
pub(crate) fn pump<S, T, U>(
    capacity: usize,
    stream: S,
    mut f: impl FnMut(T) -> Option<U> + Send + 'static,
) -> EngineStream<U>
where
    S: Stream<Item = T> + Send + 'static,
    T: Send + 'static,
    U: Send + 'static,
{
    let (mut tx, rx) = mpsc::channel(capacity);
    let task = handle().spawn(async move {
        let mut stream = std::pin::pin!(stream);
        while let Some(item) = stream.next().await {
            if let Some(out) = f(item)
                && tx.send(out).await.is_err()
            {
                break;
            }
        }
    });
    EngineStream::aborted(rx, task.abort_handle())
}

pub(crate) fn handle() -> tokio::runtime::Handle {
    RUNTIME.handle()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn dropping_receiver_aborts_silent_producer() {
        // A-001 / R-10
        let held = Arc::new(());
        let token = held.clone();
        let stream = futures::stream::poll_fn(move |_cx| {
            let _keep = &token;
            Poll::<Option<()>>::Pending
        });
        let rx = pump(8, stream, Some);
        assert_eq!(Arc::strong_count(&held), 2);
        drop(rx);
        let deadline = Instant::now() + Duration::from_millis(300);
        while Arc::strong_count(&held) > 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(Arc::strong_count(&held), 1);
    }

    #[test]
    fn external_handle_avoids_starting_fallback_runtime() {
        // P-015: injecting a host runtime handle runs tasks on it without spawning engine-io.
        let ext = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("host-rt-test")
            .enable_all()
            .build()
            .unwrap();
        let cell = RuntimeCell::new();
        cell.set_handle(ext.handle().clone());
        let thread_name = futures::executor::block_on(EngineTask(
            cell.handle()
                .spawn(async { std::thread::current().name().unwrap_or_default().to_owned() }),
        ));
        assert_eq!(thread_name, "host-rt-test");
        assert!(!cell.has_fallback());

        // Positive control: without an external handle, the fallback engine-io runtime starts.
        let fallback_cell = RuntimeCell::new();
        let fallback_name = futures::executor::block_on(EngineTask(
            fallback_cell
                .handle()
                .spawn(async { std::thread::current().name().unwrap_or_default().to_owned() }),
        ));
        assert_eq!(fallback_name, "engine-io");
        assert!(fallback_cell.has_fallback());
    }
}
