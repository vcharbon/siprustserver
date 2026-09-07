//! The in-memory subscriber tests capture logs and trace events with.
//!
//! ONE subscriber for the process, installed on first use; capture is scoped by
//! the thread-local buffer stack it writes to, so a current-thread runtime's
//! whole test body is covered and two tests running concurrently never see each
//! other's output. The subscriber must be process-wide because `tracing` caches
//! a callsite's `Interest` GLOBALLY, computed once from whatever subscriber the
//! thread that first reached that callsite happened to hold: under a
//! thread-scoped subscriber a callsite first reached by a test that captures
//! nothing is cached as never-enabled, and every concurrently capturing test
//! silently loses it. It runs no background task, performs no IO and reads no
//! wall clock — a paused-clock test installing it stays paused
//! (docs/testing/test-clock.md).
//!
//! Scenario tests never assert on captured content — the `Recorder` is the
//! oracle. Tests OF the trace machinery may.
//!
//! One buffer per thread: [`current_test_buffer`] hands the installed handle to
//! a second would-be installer (a harness constructed inside a test that already
//! captures), so nesting reuses one buffer instead of shadowing the outer one.
//! Installs are tracked as a stack keyed by handle identity, so a guard dropped
//! out of order withdraws only its own entry.

use std::cell::RefCell;
use std::sync::{Arc, Mutex, Once};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::subscriber::Interest;
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::Registry;

thread_local! {
    /// The buffers installed on this thread, oldest first; the last is the
    /// current one — see [`current_test_buffer`]. A STACK rather than one slot
    /// because guards need not drop in install order: each guard removes its
    /// OWN entry wherever it sits, so an out-of-order drop neither resurrects a
    /// buffer nobody holds nor hides the live one.
    static INSTALLED: RefCell<Vec<TestLogHandle>> = const { RefCell::new(Vec::new()) };
}

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

/// One captured span at creation: its name, target and the fields it opened
/// with. A span's own attributes — the correlation ids, the takeover link — are
/// only visible here, never on its events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedSpan {
    /// The span's name (`sip.call`, `sip.call.http`).
    pub name: String,
    /// The emitting target.
    pub target: String,
    /// Every creation field as `(name, rendered value)`, in declaration order.
    pub fields: Vec<(String, String)>,
}

impl CapturedSpan {
    /// The span rendered as one `key=value` line.
    pub fn line(&self) -> String {
        let mut s = format!("{}: {}", self.target, self.name);
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
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
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

    /// Every span captured at creation, oldest first.
    pub fn spans(&self) -> Vec<CapturedSpan> {
        self.spans.lock().expect("capture buffer mutex").clone()
    }

    /// Captured spans whose rendered line contains `needle`.
    pub fn spans_matching(&self, needle: &str) -> Vec<CapturedSpan> {
        self.spans().into_iter().filter(|s| s.contains(needle)).collect()
    }

    /// Drop everything captured so far.
    pub fn clear(&self) {
        self.events.lock().expect("capture buffer mutex").clear();
        self.spans.lock().expect("capture buffer mutex").clear();
    }
}

/// Holds one buffer installed on this thread. Capture reverts to whatever was
/// in place when this is dropped.
pub struct TestLogGuard {
    /// The handle this guard installed — the identity its removal is keyed on.
    mine: TestLogHandle,
}

impl Drop for TestLogGuard {
    /// Withdraw THIS guard's buffer, whatever position it holds. Dropping in
    /// install order pops the top and uncovers the enclosing buffer; dropping
    /// out of order removes an entry from under the live one and leaves the
    /// current buffer where it is.
    fn drop(&mut self) {
        let _ = INSTALLED.try_with(|stack| {
            let mut stack = stack.borrow_mut();
            if let Some(at) = stack.iter().rposition(|h| Arc::ptr_eq(&h.events, &self.mine.events))
            {
                stack.remove(at);
            }
        });
    }
}

/// Install the capture subscriber process-wide, once. A thread capturing
/// nothing is one predicated `enabled` call per event: the filter reads this
/// thread's buffer stack and refuses when it is empty, so no span is opened and
/// no event is built.
///
/// A subscriber another crate already made global keeps the process — capture
/// then records nothing, which is the same answer as installing over it and
/// losing that subscriber's output.
fn install_capture_subscriber() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = tracing::subscriber::set_global_default(Registry::default().with(CaptureLayer));
    });
}

/// Install a capture buffer for the current thread. Hold the guard for as long
/// as capture should run.
///
/// Captures `info` and above: the lifecycle plane and the per-call trace plane
/// both emit at `info`, while `debug`/`trace` diagnostics stay off so a long
/// scenario does not accumulate what nothing reads.
pub fn test_buffer() -> (TestLogGuard, TestLogHandle) {
    install_capture_subscriber();
    let handle = TestLogHandle::default();
    INSTALLED.with(|stack| stack.borrow_mut().push(handle.clone()));
    (TestLogGuard { mine: handle.clone() }, handle)
}

/// The buffer [`test_buffer`] installed on this thread, if one is active.
///
/// A component that wants capture but must not shadow an enclosing test's
/// buffer reuses this handle instead of installing its own.
pub fn current_test_buffer() -> Option<TestLogHandle> {
    INSTALLED.try_with(|stack| stack.borrow().last().cloned()).ok().flatten()
}

/// The capture layer: appends one [`CapturedEvent`] per event and one
/// [`CapturedSpan`] per span creation, to whichever buffer the EMITTING thread
/// has installed, and nothing else.
struct CaptureLayer;

impl<S: Subscriber> Layer<S> for CaptureLayer {
    /// `sometimes` and never `always`: the answer depends on the EMITTING
    /// thread, and an `always` would let `tracing` cache the first thread's
    /// answer for the whole process.
    fn register_callsite(&self, meta: &'static Metadata<'static>) -> Interest {
        if *meta.level() <= Level::INFO {
            Interest::sometimes()
        } else {
            Interest::never()
        }
    }

    /// Captures `info` and above, and only on a thread holding a buffer: a test
    /// capturing nothing opens no span and builds no event.
    fn enabled(&self, meta: &Metadata<'_>, _ctx: Context<'_, S>) -> bool {
        *meta.level() <= Level::INFO && current_test_buffer().is_some()
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(LevelFilter::INFO)
    }

    fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {
        let Some(handle) = current_test_buffer() else { return };
        let mut visitor = FieldVisitor::default();
        attrs.record(&mut visitor);
        let meta = attrs.metadata();
        let captured = CapturedSpan {
            name: meta.name().to_string(),
            target: meta.target().to_string(),
            fields: visitor.fields,
        };
        handle.spans.lock().expect("capture buffer mutex").push(captured);
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let Some(handle) = current_test_buffer() else { return };
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let meta = event.metadata();
        let captured = CapturedEvent {
            level: level_str(meta.level()).to_string(),
            target: meta.target().to_string(),
            message: visitor.message,
            fields: visitor.fields,
        };
        handle.events.lock().expect("capture buffer mutex").push(captured);
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
    fn a_spans_creation_fields_are_captured() {
        let (_guard, log) = test_buffer();
        let _span = tracing::info_span!("sip.call", link.span_id = "abc123", sip.call_id = "c@h");

        let spans = log.spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].name, "sip.call");
        assert!(spans[0].contains("link.span_id=abc123"));
    }

    #[test]
    fn a_nested_installer_reuses_the_outer_buffer_instead_of_shadowing_it() {
        let (_guard, outer) = test_buffer();
        let reused = current_test_buffer().expect("a buffer is installed on this thread");
        tracing::info!("one line");
        assert_eq!(reused.lines().len(), 1, "the reused handle reads the same buffer");
        assert_eq!(outer.lines().len(), 1);
    }

    #[test]
    fn no_buffer_is_current_outside_an_install() {
        assert!(current_test_buffer().is_none());
        {
            let (_guard, _log) = test_buffer();
            assert!(current_test_buffer().is_some());
        }
        assert!(current_test_buffer().is_none(), "the guard restores what it displaced");
    }

    #[test]
    fn an_out_of_order_guard_drop_leaves_the_current_buffer_alone() {
        let (first, _a) = test_buffer();
        let (second, b) = test_buffer();
        // The first guard drops while the second is still live — guards nest by
        // convention, not by construction.
        drop(first);

        let current = current_test_buffer().expect("the second install is still current");
        assert!(
            Arc::ptr_eq(&current.events, &b.events),
            "an out-of-order drop must not hand back the buffer it displaced",
        );
        drop(second);
        assert!(
            current_test_buffer().is_none(),
            "the withdrawn buffer is gone for good — no dead handle is uncovered",
        );
    }

    #[test]
    fn debug_diagnostics_are_not_accumulated() {
        let (_guard, log) = test_buffer();
        tracing::debug!("per-call diagnostic");
        tracing::info!("lifecycle");
        assert_eq!(log.lines().len(), 1);
        assert!(log.lines()[0].contains("lifecycle"));
    }

    /// One callsite, reachable from any thread — the identity `tracing` caches
    /// its `Interest` under.
    fn cold_probe() {
        tracing::info!("cold callsite");
    }

    #[test]
    fn a_callsite_first_reached_by_a_thread_that_captures_nothing_still_reaches_the_buffer() {
        let (_guard, log) = test_buffer();
        // `tracing` computes a callsite's Interest ONCE, from whatever
        // subscriber the thread that first reaches it holds, and caches it for
        // the whole process. This thread holds none.
        std::thread::spawn(cold_probe).join().expect("cold thread");

        cold_probe();
        assert_eq!(log.lines().len(), 1, "a cold registration must not silence the callsite");
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
