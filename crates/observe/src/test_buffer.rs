//! The in-memory subscriber tests capture logs and trace events with.
//!
//! Thread-scoped ([`tracing::subscriber::set_default`]), so a current-thread
//! runtime's whole test body is covered and two tests running concurrently
//! never see each other's output. It runs no background task, performs no IO
//! and reads no wall clock — a paused-clock test installing it stays paused
//! (docs/testing/test-clock.md).
//!
//! Scenario tests never assert on captured content — the `Recorder` is the
//! oracle. Tests OF the trace machinery may.

use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::subscriber::DefaultGuard;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::Registry;

/// One captured `tracing` event: its level, target, message and fields, in
/// emission order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedEvent {
    /// The event's level, rendered (`INFO`, `WARN`, …).
    pub level: String,
    /// The emitting module path.
    pub target: String,
    /// The `message` field, empty when the event carries none.
    pub message: String,
    /// Every other field as `(name, rendered value)`, in declaration order.
    pub fields: Vec<(String, String)>,
}

impl CapturedEvent {
    /// The event rendered as one `key=value` line, the same shape the
    /// production fmt layer writes.
    pub fn line(&self) -> String {
        let mut s = format!("{} {}: {}", self.level, self.target, self.message);
        for (k, v) in &self.fields {
            s.push(' ');
            s.push_str(k);
            s.push('=');
            s.push_str(v);
        }
        s
    }

    /// Whether any part of the rendered line contains `needle`.
    pub fn contains(&self, needle: &str) -> bool {
        self.line().contains(needle)
    }
}

/// A snapshot handle over the capture buffer. Cloneable and cheap; every clone
/// reads the same buffer.
#[derive(Clone, Default)]
pub struct TestLogHandle {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl TestLogHandle {
    /// Every event captured so far, oldest first.
    pub fn snapshot(&self) -> Vec<CapturedEvent> {
        self.events.lock().expect("capture buffer mutex").clone()
    }

    /// Every captured event rendered as a line.
    pub fn lines(&self) -> Vec<String> {
        self.snapshot().iter().map(CapturedEvent::line).collect()
    }

    /// Captured events whose rendered line contains `needle`.
    pub fn matching(&self, needle: &str) -> Vec<CapturedEvent> {
        self.snapshot().into_iter().filter(|e| e.contains(needle)).collect()
    }

    /// Drop everything captured so far.
    pub fn clear(&self) {
        self.events.lock().expect("capture buffer mutex").clear();
    }
}

/// Holds the thread-scoped subscriber installed. Logging reverts to whatever
/// was in place when this is dropped.
pub struct TestLogGuard {
    _inner: DefaultGuard,
}

/// Install the in-memory subscriber for the current thread. Hold the guard for
/// as long as capture should run.
pub fn test_buffer() -> (TestLogGuard, TestLogHandle) {
    let handle = TestLogHandle::default();
    let subscriber = Registry::default().with(CaptureLayer { handle: handle.clone() });
    let guard = tracing::subscriber::set_default(subscriber);
    (TestLogGuard { _inner: guard }, handle)
}

/// The capture layer: appends one [`CapturedEvent`] per event, nothing else.
struct CaptureLayer {
    handle: TestLogHandle,
}

impl<S: Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let meta = event.metadata();
        let captured = CapturedEvent {
            level: level_str(meta.level()).to_string(),
            target: meta.target().to_string(),
            message: visitor.message,
            fields: visitor.fields,
        };
        self.handle.events.lock().expect("capture buffer mutex").push(captured);
    }
}

fn level_str(level: &Level) -> &'static str {
    match *level {
        Level::TRACE => "TRACE",
        Level::DEBUG => "DEBUG",
        Level::INFO => "INFO",
        Level::WARN => "WARN",
        Level::ERROR => "ERROR",
    }
}

/// Renders each field with its `Debug`/display form, splitting out `message`.
#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: Vec<(String, String)>,
}

impl FieldVisitor {
    fn put(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = value;
        } else {
            self.fields.push((field.name().to_string(), value));
        }
    }
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.put(field, format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_captured_with_their_fields() {
        let (_guard, log) = test_buffer();
        tracing::info!(node = "w-0", peer = 3u64, "takeover complete");
        tracing::warn!(reason = "no_exporter", "trace inert");

        let events = log.snapshot();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].level, "INFO");
        assert_eq!(events[0].message, "takeover complete");
        assert_eq!(
            events[0].fields,
            vec![("node".to_string(), "w-0".to_string()), ("peer".to_string(), "3".to_string())]
        );
        assert!(events[1].contains("reason=no_exporter"));
    }

    #[test]
    fn capture_stops_when_the_guard_is_dropped() {
        let (guard, log) = test_buffer();
        tracing::info!("inside");
        drop(guard);
        tracing::info!("outside");
        assert_eq!(log.lines().len(), 1, "only the in-scope event is captured");
        assert!(log.lines()[0].contains("inside"));
    }
}
