use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, Once};
use std::time::Instant;

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

#[cfg(test)]
std::thread_local! {
    static CAPTURED: std::cell::RefCell<Option<Vec<(Level, String)>>> = const { std::cell::RefCell::new(None) };
}

struct LineVisitor {
    message: String,
    fields: String,
}

impl LineVisitor {
    fn new() -> Self {
        Self {
            message: String::new(),
            fields: String::new(),
        }
    }

    fn finish(self) -> String {
        if self.fields.is_empty() {
            self.message
        } else if self.message.is_empty() {
            self.fields
        } else {
            format!("{} {}", self.message, self.fields)
        }
    }
}

impl Visit for LineVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{}={value}", field.name());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{}={value:?}", field.name());
        }
    }
}

struct SpanEntry {
    name: &'static str,
    target: &'static str,
    level: Level,
    fields: String,
    started_at: Instant,
}

struct StderrSubscriber {
    max_level: Level,
    next_id: AtomicU64,
    spans: Mutex<HashMap<u64, SpanEntry>>,
}

impl StderrSubscriber {
    fn from_env() -> Self {
        let max_level = match std::env::var("RUST_LOG")
            .ok()
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("trace") => Level::TRACE,
            Some("debug") => Level::DEBUG,
            Some("warn") => Level::WARN,
            Some("error") => Level::ERROR,
            _ => Level::INFO,
        };
        Self {
            max_level,
            next_id: AtomicU64::new(1),
            spans: Mutex::new(HashMap::new()),
        }
    }

    fn emit_line(&self, level: Level, line: String) {
        #[cfg(test)]
        let captured = CAPTURED.with(|c| {
            if let Some(buf) = c.borrow_mut().as_mut() {
                buf.push((level, line.clone()));
                true
            } else {
                false
            }
        });
        #[cfg(not(test))]
        let captured = false;

        if !captured && level <= self.max_level {
            let _ = writeln!(std::io::stderr().lock(), "{line}");
        }
    }
}

impl Subscriber for StderrSubscriber {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        #[cfg(test)]
        if CAPTURED.with(|c| c.borrow().is_some()) {
            return *metadata.level() <= Level::DEBUG;
        }
        *metadata.level() <= self.max_level
    }

    fn new_span(&self, span: &Attributes<'_>) -> Id {
        let raw = self.next_id.fetch_add(1, Ordering::Relaxed);
        let meta = span.metadata();
        if self.enabled(meta) {
            let mut visitor = LineVisitor::new();
            span.record(&mut visitor);
            self.spans.lock().expect("span lock").insert(
                raw,
                SpanEntry {
                    name: meta.name(),
                    target: meta.target(),
                    level: *meta.level(),
                    fields: visitor.finish(),
                    started_at: Instant::now(),
                },
            );
        }
        Id::from_u64(raw)
    }

    fn record(&self, span: &Id, values: &Record<'_>) {
        let mut spans = self.spans.lock().expect("span lock");
        if let Some(entry) = spans.get_mut(&span.into_u64()) {
            let mut visitor = LineVisitor {
                message: String::new(),
                fields: std::mem::take(&mut entry.fields),
            };
            values.record(&mut visitor);
            entry.fields = visitor.finish();
        }
    }

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut visitor = LineVisitor::new();
        event.record(&mut visitor);
        let body = visitor.finish();
        let meta = event.metadata();
        let line = format!("[{} {}] {body}", meta.level(), meta.target());
        self.emit_line(*meta.level(), line);
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}

    fn try_close(&self, id: Id) -> bool {
        let entry = self.spans.lock().expect("span lock").remove(&id.into_u64());
        if let Some(entry) = entry {
            let elapsed_us = entry.started_at.elapsed().as_micros();
            let line = if entry.fields.is_empty() {
                format!(
                    "[{} {}] span={} elapsed_us={elapsed_us}",
                    entry.level, entry.target, entry.name
                )
            } else {
                format!(
                    "[{} {}] span={} {} elapsed_us={elapsed_us}",
                    entry.level, entry.target, entry.name, entry.fields
                )
            };
            self.emit_line(entry.level, line);
        }
        true
    }
}

pub fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing::subscriber::set_global_default(StderrSubscriber::from_env());
    });
}

#[cfg(test)]
pub fn capture<R>(f: impl FnOnce() -> R) -> (R, Vec<(Level, String)>) {
    init();
    CAPTURED.with(|c| {
        *c.borrow_mut() = Some(Vec::new());
    });
    let out = f();
    let events = CAPTURED.with(|c| c.borrow_mut().take().unwrap_or_default());
    (out, events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscriber_records_span_fields_and_elapsed_microseconds() {
        // P-016: debug spans record fields and elapsed_us on close for diagnostics.
        let ((), lines) = capture(|| {
            let span = tracing::debug_span!(
                "refresh_list",
                kind = "container",
                items = tracing::field::Empty
            );
            let _enter = span.enter();
            span.record("items", 12usize);
        });
        assert!(
            lines.iter().any(|(lvl, l)| {
                *lvl == Level::DEBUG
                    && l.contains("span=refresh_list")
                    && l.contains("kind=container")
                    && l.contains("items=12")
                    && l.contains("elapsed_us=")
            }),
            "{lines:?}"
        );
    }
}
