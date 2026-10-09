//! What the SUT logged and traced while a scenario ran: the capture buffer a
//! [`Harness`](super::Harness) holds for its life, and the tail of lifecycle
//! log lines a failure dump prints. The per-call trace plane is rendered apart
//! ([`super::call_trace`]).
//!
//! Capture is the thread-scoped in-memory subscriber from `observe` — no
//! background task, no IO, no wall-clock signal, so a paused-clock test stays
//! paused (docs/testing/test-clock.md). A clean run discards the buffer.
//!
//! Scenario tests never assert on captured content — the `Recorder` is their
//! oracle; this capture exists only so a failing scenario self-documents.

use observe::{CapturedEvent, TestLogGuard, TestLogHandle};

/// How many lifecycle log lines a dump prints. A dump is read by a human right
/// after the digest, so it shows the tail — the lines nearest the failure.
const DUMP_TAIL_LINES: usize = 200;

/// The thread's capture buffer for the lifetime of a
/// [`Harness`](super::Harness).
///
/// When a test already captures — a dedicated test OF the trace machinery — the
/// enclosing buffer is reused rather than shadowed, so that test still reads
/// everything it asserts on.
pub(super) struct LogCapture {
    handle: TestLogHandle,
    /// `Some` only when this capture is the one that installed the subscriber.
    _installed: Option<TestLogGuard>,
}

impl LogCapture {
    /// Start capturing on this thread.
    pub(super) fn install() -> Self {
        let (installed, handle) = match observe::current_test_buffer() {
            Some(handle) => (None, handle),
            None => {
                let (guard, handle) = observe::test_buffer();
                (Some(guard), handle)
            }
        };
        Self { handle, _installed: installed }
    }

    /// Every event captured so far, oldest first.
    pub(super) fn events(&self) -> Vec<CapturedEvent> {
        self.handle.snapshot()
    }
}

/// The tail of the lifecycle log lines among `events` (the trace plane
/// excluded), oldest of the shown lines first, and how many there were.
pub(super) fn render_log_tail(events: &[CapturedEvent]) -> (Vec<String>, usize) {
    let lines: Vec<String> =
        events.iter().filter(|e| !observe::is_trace_plane(&e.target)).map(|e| e.line()).collect();
    let mut out = Vec::new();
    if lines.is_empty() {
        out.push("(nothing logged)".to_string());
    }
    let elided = lines.len().saturating_sub(DUMP_TAIL_LINES);
    if elided > 0 {
        out.push(format!("… {elided} earlier line(s) elided"));
    }
    out.extend(lines.iter().skip(elided).cloned());
    (out, lines.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tail_renders_the_captured_lifecycle_lines() {
        let capture = LogCapture::install();
        tracing::info!(node = "w-0", "takeover complete");
        let (lines, n) = render_log_tail(&capture.events());
        assert_eq!(n, 1);
        assert!(lines[0].contains("takeover complete"), "{lines:?}");
        assert!(lines[0].contains("node=w-0"), "{lines:?}");
    }

    #[test]
    fn the_tail_says_how_much_it_elided() {
        let capture = LogCapture::install();
        for i in 0..(DUMP_TAIL_LINES + 5) {
            tracing::info!(i, "line");
        }
        let (lines, _) = render_log_tail(&capture.events());
        assert!(lines[0].contains("5 earlier line(s) elided"), "{lines:?}");
        let last = lines.last().unwrap();
        assert!(last.contains(&format!("i={}", DUMP_TAIL_LINES + 4)), "the tail is shown: {last}");
    }

    #[test]
    fn the_trace_plane_is_not_in_the_tail() {
        let capture = LogCapture::install();
        tracing::info!(target: observe::TRACE_TARGET, kind = "rule.fired", "trace");
        let (lines, n) = render_log_tail(&capture.events());
        assert_eq!(n, 0, "{lines:?}");
    }

    #[test]
    fn an_enclosing_capture_buffer_is_reused_not_shadowed() {
        let (_guard, log) = observe::test_buffer();
        let capture = LogCapture::install();
        tracing::info!("visible to both");
        assert_eq!(log.lines().len(), 1, "the test's own buffer still sees the line");
        assert!(render_log_tail(&capture.events()).0[0].contains("visible to both"));
    }
}
