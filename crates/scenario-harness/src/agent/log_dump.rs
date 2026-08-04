//! The panic-time log dump: what the SUT logged and traced while the scenario
//! ran, printed alongside [`PanicDump`](super::run_guards::PanicDump)'s wire
//! trace when a scenario unwinds.
//!
//! Capture is the thread-scoped in-memory subscriber from `observe` — no
//! background task, no IO, no wall-clock signal, so a paused-clock test stays
//! paused (docs/testing/test-clock.md). A clean run discards the buffer.
//!
//! Scenario tests never assert on captured content — the `Recorder` is their
//! oracle; this guard exists only so a failing scenario self-documents.

use std::cell::Cell;

use observe::{TestLogGuard, TestLogHandle};

/// How many captured lines a dump prints. A dump is read by a human right after
/// the panic message, so it shows the tail — the lines nearest the failure.
const DUMP_TAIL_LINES: usize = 200;

/// RAII log dumper: installs (or joins) the thread's capture buffer for the
/// lifetime of a [`Harness`](super::Harness) and, if the scenario task unwinds
/// before `finish`, writes the captured lines to stderr.
///
/// When a test already captures — a dedicated test OF the trace machinery — the
/// enclosing buffer is reused rather than shadowed, so that test still reads
/// everything it asserts on.
pub(super) struct LogDump {
    name: String,
    handle: TestLogHandle,
    /// `Some` only when this guard is the one that installed the subscriber.
    _installed: Option<TestLogGuard>,
    armed: Cell<bool>,
}

impl LogDump {
    /// Start capturing for a scenario named `name`.
    pub(super) fn install(name: String) -> Self {
        let (installed, handle) = match observe::current_test_buffer() {
            Some(handle) => (None, handle),
            None => {
                let (guard, handle) = observe::test_buffer();
                (Some(guard), handle)
            }
        };
        Self { name, handle, _installed: installed, armed: Cell::new(true) }
    }

    /// Stop the dump from firing — the run reported through its normal path.
    pub(super) fn disarm(&self) {
        self.armed.set(false);
    }

    /// Render the captured tail, oldest of the shown lines first.
    fn render(&self) -> String {
        let lines = self.handle.lines();
        let mut out = format!(
            "\n══ SUT log/trace for '{}' (dumped on panic — finish() not reached) ══\n",
            self.name
        );
        if lines.is_empty() {
            out.push_str("  (nothing logged)\n");
        }
        let elided = lines.len().saturating_sub(DUMP_TAIL_LINES);
        if elided > 0 {
            out.push_str(&format!("  … {elided} earlier line(s) elided\n"));
        }
        for line in lines.iter().skip(elided) {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(&format!("══ end SUT log/trace ({} line(s)) ══\n", lines.len()));
        out
    }
}

impl Drop for LogDump {
    fn drop(&mut self) {
        if !self.armed.get() || !std::thread::panicking() {
            return;
        }
        // Never panic while already unwinding: a second panic in `Drop` aborts
        // the process. Swallow any failure (e.g. a poisoned capture mutex).
        if let Ok(text) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.render())) {
            eprint!("{text}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dump_renders_the_captured_lifecycle_lines() {
        let dump = LogDump::install("scenario".to_string());
        tracing::info!(node = "w-0", "takeover complete");
        let text = dump.render();
        assert!(text.contains("takeover complete"), "{text}");
        assert!(text.contains("node=w-0"), "{text}");
        dump.disarm();
    }

    #[test]
    fn a_dump_shows_the_tail_and_says_how_much_it_elided() {
        let dump = LogDump::install("noisy".to_string());
        for i in 0..(DUMP_TAIL_LINES + 5) {
            tracing::info!(i, "line");
        }
        let text = dump.render();
        assert!(text.contains("5 earlier line(s) elided"), "{text}");
        assert!(text.contains(&format!("i={}", DUMP_TAIL_LINES + 4)), "the tail is shown: {text}");
        dump.disarm();
    }

    #[test]
    fn an_enclosing_capture_buffer_is_reused_not_shadowed() {
        let (_guard, log) = observe::test_buffer();
        let dump = LogDump::install("nested".to_string());
        tracing::info!("visible to both");
        assert_eq!(log.lines().len(), 1, "the test's own buffer still sees the line");
        assert!(dump.render().contains("visible to both"));
        dump.disarm();
    }
}
