//! Live streams to the UI: logs, stats, exec.
//! Each has an id; `unsubscribe` (or a failed send, i.e. the window went away)
//! ends it. Output is batched so the UI's cost does not scale with chunk count.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use engine::{Engine, ExecControl, ExecInput, ExecSpec, TtySize};
use futures::{Stream, StreamExt};
use tokio::task::JoinHandle;

use crate::dto::{ExecOutput, LogBatch, LogChunk, LogStream, StatsEvent, UiError};

const LOG_FLUSH_EVERY: Duration = Duration::from_millis(50);
const LOG_BATCH_MAX_BYTES: usize = 128 * 1024;
const EXEC_FLUSH_EVERY: Duration = Duration::from_millis(16);
const EXEC_FLUSH_BYTES: usize = 64 * 1024;
pub const STREAM_MAX_IN_FLIGHT: usize = 4;

#[derive(Default)]
pub(crate) struct FlowControl {
    in_flight: AtomicUsize,
    notify: tokio::sync::Notify,
}

impl FlowControl {
    fn can_send(&self) -> bool {
        self.in_flight.load(Ordering::Acquire) < STREAM_MAX_IN_FLIGHT
    }

    fn acquire_one(&self) {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn ack(&self, credits: u32) {
        if credits == 0 {
            return;
        }
        let n = credits as usize;
        let mut cur = self.in_flight.load(Ordering::Acquire);
        loop {
            let next = cur.saturating_sub(n);
            match self.in_flight.compare_exchange_weak(
                cur,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
        self.notify.notify_waiters();
    }

    #[cfg(test)]
    fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }
}

struct Sub {
    task: JoinHandle<()>,
    exec_input: Option<ExecControl>,
    flow: Option<Arc<FlowControl>>,
}

#[derive(Default)]
struct StreamsInner {
    next: AtomicU32,
    subs: Mutex<HashMap<u32, Sub>>,
}

#[derive(Clone, Default)]
pub struct Streams(Arc<StreamsInner>);

struct SubCleanupGuard {
    streams: Streams,
    id: u32,
}

impl Drop for SubCleanupGuard {
    fn drop(&mut self) {
        self.streams
            .0
            .subs
            .lock()
            .expect("streams lock")
            .remove(&self.id);
    }
}

async fn pump_stats<S, F>(mut rx: S, mut send: F)
where
    S: Stream<Item = Result<engine::StatsSample, engine::EngineError>> + Unpin,
    F: FnMut(StatsEvent) -> bool,
{
    while let Some(item) = rx.next().await {
        match item {
            Ok(s) => {
                if !send(StatsEvent::Sample(s.into())) {
                    return;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "stats stream ended with error");
                let _ = send(StatsEvent::Ended {
                    error: Some(e.to_string()),
                });
                return;
            }
        }
    }
    let _ = send(StatsEvent::Ended { error: None });
}

fn interval_ticks(period: Duration) -> impl Stream<Item = ()> + Unpin {
    Box::pin(futures::stream::unfold(
        None,
        move |state: Option<tokio::time::Interval>| async move {
            let mut tick = match state {
                Some(t) => t,
                None => {
                    let mut t = tokio::time::interval(period);
                    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    t.tick().await;
                    t
                }
            };
            tick.tick().await;
            Some(((), Some(tick)))
        },
    ))
}

async fn pump_logs<S, T, F>(mut rx: S, mut ticks: T, flow: Arc<FlowControl>, mut send: F)
where
    S: Stream<Item = Result<engine::LogChunk, engine::EngineError>> + Unpin,
    T: Stream<Item = ()> + Unpin,
    F: FnMut(LogBatch) -> bool,
{
    let mut batch = Batch::default();
    let mut tick_due = false;

    loop {
        let notified = flow.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let window_open = flow.can_send();

        if tick_due && window_open && !batch.chunks.is_empty() {
            let bytes = batch.bytes;
            let out = batch.take(None);
            flow.acquire_one();
            tick_due = false;
            tracing::trace!(bytes, "flushed log batch");
            if !send(out) {
                return;
            }
            continue;
        }

        let can_read = window_open && batch.bytes < LOG_BATCH_MAX_BYTES;
        tokio::select! {
            biased;
            item = rx.next(), if can_read => match item {
                Some(Ok(c)) => batch.push(c.stream, c.text),
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "log stream ended with error");
                    let _ = send(batch.take(Some(e.to_string())));
                    return;
                }
                None => {
                    let _ = send(batch.take(Some("Log stream ended (container stopped).".into())));
                    return;
                }
            },
            t = ticks.next() => {
                if t.is_none() {
                    return;
                }
                if !batch.chunks.is_empty() {
                    if flow.can_send() {
                        let bytes = batch.bytes;
                        let out = batch.take(None);
                        flow.acquire_one();
                        tick_due = false;
                        tracing::trace!(bytes, "flushed log batch");
                        if !send(out) {
                            return;
                        }
                    } else {
                        tick_due = true;
                    }
                }
            }
            _ = &mut notified, if !window_open => {}
        }
    }
}

async fn pump_exec<S, T, F>(mut output: S, mut ticks: T, flow: Arc<FlowControl>, mut send: F)
where
    S: Stream<Item = Vec<u8>> + Unpin,
    T: Stream<Item = ()> + Unpin,
    F: FnMut(ExecOutput) -> bool,
{
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut pending: Vec<u8> = Vec::new();
    let mut tick_due = false;

    let flush_one = |pending: &mut Vec<u8>, flow: &FlowControl, send: &mut F| -> bool {
        let take_len = pending.len().min(EXEC_FLUSH_BYTES);
        let encoded = b64.encode(&pending[..take_len]);
        if take_len == pending.len() {
            pending.clear();
            if pending.capacity() > EXEC_FLUSH_BYTES * 2 {
                pending.shrink_to(EXEC_FLUSH_BYTES);
            }
        } else {
            pending.drain(..take_len);
        }
        flow.acquire_one();
        tracing::trace!(bytes = take_len, "flushed exec chunk");
        send(ExecOutput::Data { base64: encoded })
    };

    loop {
        let notified = flow.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let window_open = flow.can_send();

        if tick_due && window_open && !pending.is_empty() {
            tick_due = false;
            if !flush_one(&mut pending, &flow, &mut send) {
                return;
            }
            continue;
        }

        let can_read = window_open && pending.len() < EXEC_FLUSH_BYTES;
        tokio::select! {
            biased;
            chunk = output.next(), if can_read => match chunk {
                Some(bytes) => {
                    if bytes.is_empty() {
                        continue;
                    }
                    if pending.is_empty() {
                        pending = bytes;
                    } else {
                        pending.extend_from_slice(&bytes);
                    }
                }
                None => {
                    while !pending.is_empty() {
                        let take_len = pending.len().min(EXEC_FLUSH_BYTES);
                        let encoded = b64.encode(&pending[..take_len]);
                        pending.drain(..take_len);
                        if !send(ExecOutput::Data { base64: encoded }) {
                            return;
                        }
                    }
                    let _ = send(ExecOutput::Exit);
                    return;
                }
            },
            t = ticks.next() => {
                if t.is_none() {
                    return;
                }
                if !pending.is_empty() {
                    if flow.can_send() {
                        tick_due = false;
                        if !flush_one(&mut pending, &flow, &mut send) {
                            return;
                        }
                    } else {
                        tick_due = true;
                    }
                }
            }
            _ = &mut notified, if !window_open => {}
        }
    }
}

impl Streams {
    fn id(&self) -> u32 {
        self.0.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn spawn_registered<F>(
        &self,
        exec_input: Option<ExecControl>,
        flow: Option<Arc<FlowControl>>,
        make: F,
    ) -> u32
    where
        F: FnOnce(SubCleanupGuard) -> JoinHandle<()>,
    {
        let id = self.id();
        let mut subs = self.0.subs.lock().expect("streams lock");
        let guard = SubCleanupGuard {
            streams: self.clone(),
            id,
        };
        let task = make(guard);
        subs.insert(
            id,
            Sub {
                task,
                exec_input,
                flow,
            },
        );
        id
    }

    pub fn ack(&self, id: u32, credits: u32) {
        let flow = self
            .0
            .subs
            .lock()
            .expect("streams lock")
            .get(&id)
            .and_then(|s| s.flow.clone());
        if let Some(flow) = flow {
            flow.ack(credits);
        }
    }

    pub fn unsubscribe(&self, id: u32) {
        let sub = self.0.subs.lock().expect("streams lock").remove(&id);
        if let Some(sub) = sub {
            sub.task.abort();
        }
    }

    pub fn clear(&self) {
        let subs: Vec<Sub> = self
            .0
            .subs
            .lock()
            .expect("streams lock")
            .drain()
            .map(|(_, s)| s)
            .collect();
        for sub in subs {
            sub.task.abort();
        }
    }

    /// A subscription that sends nothing until it is released (for tests of who
    /// releases what). Needs a Tokio runtime.
    #[cfg(test)]
    pub fn idle(&self) -> u32 {
        self.spawn_registered(None, None, |guard| {
            tokio::spawn(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            })
        })
    }

    #[cfg(test)]
    pub fn active_count(&self) -> usize {
        self.0.subs.lock().expect("streams lock").len()
    }

    pub fn exec_send(&self, id: u32, input: ExecInput) -> Result<(), UiError> {
        let ctrl = self
            .0
            .subs
            .lock()
            .expect("streams lock")
            .get(&id)
            .and_then(|s| s.exec_input.clone())
            .ok_or_else(|| UiError {
                kind: crate::dto::UiErrorKind::NotFound,
                message: "The exec session has ended.".into(),
            })?;
        ctrl.send(input).map_err(UiError::from)
    }

    pub fn logs(
        &self,
        engine: Engine,
        container: String,
        tail: usize,
        make_send: impl FnOnce(u32) -> Box<dyn FnMut(LogBatch) -> bool + Send>,
    ) -> u32 {
        let flow = Arc::new(FlowControl::default());
        let task_flow = flow.clone();
        self.spawn_registered(None, Some(flow), |guard| {
            let send = make_send(guard.id);
            tokio::spawn(async move {
                let _guard = guard;
                let rx = engine.logs(&container, tail, true);
                pump_logs(rx, interval_ticks(LOG_FLUSH_EVERY), task_flow, send).await;
            })
        })
    }

    pub fn stats(
        &self,
        engine: Engine,
        container: String,
        make_send: impl FnOnce(u32) -> Box<dyn FnMut(StatsEvent) -> bool + Send>,
    ) -> u32 {
        self.spawn_registered(None, None, |guard| {
            let send = make_send(guard.id);
            tokio::spawn(async move {
                let _guard = guard;
                let rx = engine.stats(&container);
                pump_stats(rx, send).await;
            })
        })
    }

    pub async fn exec(
        &self,
        engine: Engine,
        container: String,
        spec: ExecSpec,
        size: TtySize,
        make_send: impl FnOnce(u32) -> Box<dyn FnMut(ExecOutput) -> bool + Send>,
    ) -> Result<u32, UiError> {
        let session = engine.exec(&container, spec, size).await?;
        let output = session.output;
        let flow = Arc::new(FlowControl::default());
        let task_flow = flow.clone();
        let id = self.spawn_registered(Some(session.input), Some(flow), |guard| {
            let send = make_send(guard.id);
            tokio::spawn(async move {
                let _guard = guard;
                pump_exec(output, interval_ticks(EXEC_FLUSH_EVERY), task_flow, send).await;
            })
        });
        Ok(id)
    }
}

/// Log chunks accumulated between flushes; consecutive chunks of one stream merge.
#[derive(Default)]
struct Batch {
    chunks: Vec<LogChunk>,
    bytes: usize,
}

impl Batch {
    fn push(&mut self, stream: engine::LogStream, text: String) {
        let stream = match stream {
            engine::LogStream::Stdout => LogStream::Stdout,
            engine::LogStream::Stderr => LogStream::Stderr,
            engine::LogStream::Console => LogStream::Console,
        };
        self.bytes += text.len();
        match self.chunks.last_mut() {
            Some(last) if last.stream == stream => last.text.push_str(&text),
            _ => self.chunks.push(LogChunk { stream, text }),
        }
    }

    fn take(&mut self, ended: Option<String>) -> LogBatch {
        self.bytes = 0;
        LogBatch {
            chunks: std::mem::take(&mut self.chunks),
            ended,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_merges_consecutive_chunks_per_stream() {
        let mut b = Batch::default();
        b.push(engine::LogStream::Stdout, "a\n".into());
        b.push(engine::LogStream::Stdout, "b\n".into());
        b.push(engine::LogStream::Stderr, "e\n".into());
        assert_eq!(b.bytes, 6);
        let out = b.take(None);
        assert_eq!(out.chunks.len(), 2);
        assert_eq!(out.chunks[0].text, "a\nb\n");
        assert_eq!(out.chunks[1].stream, LogStream::Stderr);
        assert_eq!(b.bytes, 0);
        assert!(b.chunks.is_empty());
    }

    #[tokio::test]
    async fn stats_forwards_errors_and_eof_to_channel() {
        // A-005 / R-05
        let sample = engine::StatsSample {
            cpu_percent: 12.5,
            mem_usage: 1024,
            mem_limit: 4096,
            net_rx: 10,
            net_tx: 20,
            pids: 3,
        };
        let rx = futures::stream::iter(vec![
            Ok(sample),
            Err(engine::EngineError::Transport("socket closed".into())),
        ]);
        let mut sent = Vec::new();
        pump_stats(rx, |ev| {
            sent.push(ev);
            true
        })
        .await;
        assert_eq!(sent.len(), 2);
        assert!(matches!(sent[0], StatsEvent::Sample(_)));
        assert_eq!(
            sent[1],
            StatsEvent::Ended {
                error: Some("Connection error: socket closed".into())
            }
        );
    }

    #[tokio::test]
    async fn streams_self_clean_on_exit_and_clear_aborts_active_tasks() {
        // A-001 / R-10
        let streams = Streams::default();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        streams.spawn_registered(None, None, |guard| {
            tokio::spawn(async move {
                let _guard = guard;
                let _ = done_tx.send(());
            })
        });
        let _ = done_rx.await;
        for _ in 0..20 {
            if streams.active_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(streams.active_count(), 0);

        let token = Arc::new(());
        let held = token.clone();
        streams.spawn_registered(None, None, |guard| {
            tokio::spawn(async move {
                let _guard = guard;
                let _keep = held;
                futures::future::pending::<()>().await;
            })
        });
        assert_eq!(streams.active_count(), 1);
        streams.clear();
        for _ in 0..20 {
            if Arc::strong_count(&token) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(streams.active_count(), 0);
        assert_eq!(Arc::strong_count(&token), 1);
    }

    /// A log stream that ends hands over what it had read with the reason, in one
    /// last batch, without waiting for a tick.
    #[tokio::test]
    async fn a_log_stream_that_ends_says_why_with_its_last_lines() {
        let chunk = |text: &str| {
            Ok(engine::LogChunk {
                stream: engine::LogStream::Stdout,
                text: text.into(),
            })
        };
        let cases = [
            (
                vec![
                    chunk("a\n"),
                    Err(engine::EngineError::Transport("reset".into())),
                ],
                "Connection error: reset",
            ),
            (vec![chunk("a\n")], "Log stream ended (container stopped)."),
        ];
        for (items, reason) in cases {
            let mut sent = Vec::new();
            pump_logs(
                Box::pin(futures::stream::iter(items)),
                futures::stream::pending(),
                Arc::new(FlowControl::default()),
                |b| {
                    sent.push(b);
                    true
                },
            )
            .await;
            assert_eq!(sent.len(), 1, "{reason}");
            assert_eq!(sent[0].ended.as_deref(), Some(reason));
            assert_eq!(sent[0].chunks.len(), 1);
            assert_eq!(sent[0].chunks[0].text, "a\n");
        }
    }

    #[tokio::test]
    async fn log_stream_rate_limits_bursts_and_halts_on_unacked_window() {
        // P-001: 1 MiB flood flushes at most once per tick window and pauses after STREAM_MAX_IN_FLIGHT=4
        // unacked batches until ack_stream releases credits, losing zero bytes.
        let chunk_size = 32 * 1024;
        let total_chunks = 32usize; // 1 MiB total -> 8 batches of 128 KiB
        let items: Vec<_> = (0..total_chunks)
            .map(|i| {
                Ok(engine::LogChunk {
                    stream: engine::LogStream::Stdout,
                    text: format!("{i:04}:{}", "x".repeat(chunk_size - 6)) + "\n",
                })
            })
            .collect();

        let (tick_tx, tick_rx) = futures::channel::mpsc::unbounded::<()>();
        let flow = Arc::new(FlowControl::default());
        let sent: Arc<Mutex<Vec<LogBatch>>> = Arc::new(Mutex::new(Vec::new()));
        let sent_cl = sent.clone();
        let flow_cl = flow.clone();
        let task = tokio::spawn(async move {
            pump_logs(
                Box::pin(futures::stream::iter(items).chain(futures::stream::pending())),
                tick_rx,
                flow_cl,
                move |b| {
                    sent_cl.lock().unwrap().push(b);
                    true
                },
            )
            .await;
        });

        // Drain initial 128 KiB into batch; before any tick fires, 0 IPC batches are emitted!
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(sent.lock().unwrap().len(), 0);

        // One tick emits exactly 1 coalesced 128 KiB batch (not 16 unthrottled 64 KiB batches).
        tick_tx.unbounded_send(()).unwrap();
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(sent.lock().unwrap().len(), 1);
        assert_eq!(
            sent.lock().unwrap()[0].chunks[0].text.len(),
            LOG_BATCH_MAX_BYTES
        );

        // Without acks, firing 20 ticks stops strictly at STREAM_MAX_IN_FLIGHT (4 batches).
        for _ in 0..20 {
            tick_tx.unbounded_send(()).unwrap();
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
        }
        assert_eq!(sent.lock().unwrap().len(), STREAM_MAX_IN_FLIGHT);
        assert_eq!(flow.in_flight(), STREAM_MAX_IN_FLIGHT);

        // Positive control: acknowledging credits resumes delivery for the remaining 4 batches.
        flow.ack(STREAM_MAX_IN_FLIGHT as u32);
        for _ in 0..6 {
            tick_tx.unbounded_send(()).unwrap();
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
        }
        assert_eq!(sent.lock().unwrap().len(), 8);
        let delivered_bytes: usize = sent
            .lock()
            .unwrap()
            .iter()
            .flat_map(|b| &b.chunks)
            .map(|c| c.text.len())
            .sum();
        assert_eq!(delivered_bytes, total_chunks * chunk_size);
        task.abort();
    }

    #[tokio::test]
    async fn exec_stream_caps_chunk_bytes_and_respects_ack_window() {
        // P-001 & P-014: exec chunks cap at EXEC_FLUSH_BYTES (64 KiB) and pause at STREAM_MAX_IN_FLIGHT.
        let b64 = base64::engine::general_purpose::STANDARD;
        let big = vec![b'z'; EXEC_FLUSH_BYTES * 6]; // 384 KiB single burst -> 6 frames of 64 KiB
        let stream = futures::stream::iter(vec![big]).chain(futures::stream::pending());

        let (tick_tx, tick_rx) = futures::channel::mpsc::unbounded::<()>();
        let flow = Arc::new(FlowControl::default());
        let sent: Arc<Mutex<Vec<ExecOutput>>> = Arc::new(Mutex::new(Vec::new()));
        let sent_cl = sent.clone();
        let flow_cl = flow.clone();
        let task = tokio::spawn(async move {
            pump_exec(Box::pin(stream), tick_rx, flow_cl, move |o| {
                sent_cl.lock().unwrap().push(o);
                true
            })
            .await;
        });

        for _ in 0..10 {
            tokio::task::yield_now().await;
            tick_tx.unbounded_send(()).unwrap();
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
        }
        assert_eq!(sent.lock().unwrap().len(), STREAM_MAX_IN_FLIGHT);
        for frame in sent.lock().unwrap().iter() {
            let ExecOutput::Data { base64 } = frame else {
                panic!("expected Data frame");
            };
            assert_eq!(b64.decode(base64).unwrap().len(), EXEC_FLUSH_BYTES);
        }

        flow.ack(STREAM_MAX_IN_FLIGHT as u32);
        for _ in 0..4 {
            tick_tx.unbounded_send(()).unwrap();
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
        }
        assert_eq!(sent.lock().unwrap().len(), 6);
        task.abort();
    }
}
