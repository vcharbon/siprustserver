//! The failure dump of a run: when a scenario unwinds before
//! [`Harness::finish`](super::Harness::finish) returns, stderr gets one digest
//! line saying why, then the wire ladder, each sampled call's SUT trace, what
//! a SUT adds through a [`FailureView`] (its timer ledger), every simulated
//! UA's dialog table and verdicts, and the tail of what the SUT logged.
//!
//! The digest is the first line of the panic that failed the run, unless a
//! [`FailureView`] names an earlier cause the scenario could not see (a SUT
//! turn whose panic its dispatcher isolated): that cause leads, and the panic
//! follows as what the scenario then met.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::rc::Rc;
use std::sync::Arc;

use futures::FutureExt;

use layer_harness::{Channel, Recorder};
use sip_net_testkit::SignalingNetworkEvent;

use super::call_trace::render_call_traces;
use super::ladder::render_ladder;
use super::log_dump::{render_log_tail, LogCapture};
use crate::dialogs::DialogJudge;

/// One titled block of a failure dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpSection {
    pub title: String,
    pub lines: Vec<String>,
}

/// What a SUT bound on a harness adds to that run's failure dump, registered
/// with [`Harness::add_failure_view`](super::Harness::add_failure_view).
pub trait FailureView {
    /// A failure the SUT met out of the scenario's sight — a turn's panic its
    /// dispatcher isolated — that explains whatever the scenario then met.
    /// The first view's cause leads the dump's digest, and
    /// [`Harness::finish`](super::Harness::finish) fails on it.
    fn cause(&self) -> Option<String>;

    /// The sections this SUT adds, printed after the per-call trace.
    fn sections(&self) -> Vec<DumpSection>;
}

thread_local! {
    /// Where this thread's failure dumps go instead of stderr, while
    /// [`expect_failure`] runs.
    static SINK: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

/// Write one dump: to the [`expect_failure`] sink when one is set, else to
/// stderr.
fn emit(text: String) {
    let routed = SINK.with(|sink| match sink.borrow_mut().as_mut() {
        Some(dumps) => {
            dumps.push(text.clone());
            true
        }
        None => false,
    });
    if !routed {
        eprint!("{text}");
    }
}

/// What a run that failed on purpose left: its panic message and the failure
/// dumps its harnesses wrote.
#[derive(Debug, Clone)]
pub struct FailedRun {
    pub panic: String,
    pub dumps: Vec<String>,
}

impl FailedRun {
    /// The first line of the panic message: the run's digest.
    pub fn digest(&self) -> &str {
        first_line(&self.panic)
    }
}

/// Run `scenario` expecting it to fail, for a test of the failure output
/// itself: its panic is caught and returned with the failure dumps the
/// harnesses it built wrote on this thread, routed here instead of stderr.
/// Panics if the scenario passes.
pub async fn expect_failure(scenario: impl Future<Output = ()>) -> FailedRun {
    SINK.with(|sink| *sink.borrow_mut() = Some(Vec::new()));
    let outcome = AssertUnwindSafe(scenario).catch_unwind().await;
    let dumps = SINK.with(|sink| sink.borrow_mut().take()).unwrap_or_default();
    match outcome {
        Ok(()) => panic!("the scenario was expected to fail and passed"),
        Err(payload) => {
            let panic = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            FailedRun { panic, dumps }
        }
    }
}

/// The failure views a harness holds, shared with its dump.
pub(super) type FailureViews = Rc<RefCell<Vec<Rc<dyn FailureView>>>>;
/// The dialog judges of the UAs a harness bound, shared with its dump.
pub(super) type Judges = Rc<RefCell<Vec<Arc<DialogJudge>>>>;

/// The first line of `text`.
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default().trim_end()
}

/// The first cause any of `views` names.
pub(super) fn first_cause(views: &FailureViews) -> Option<String> {
    views.borrow().iter().find_map(|v| v.cause())
}

/// The digest of a failing run: the line saying why, then what followed when
/// a view's cause leads.
fn digest(panic: Option<&str>, cause: Option<&str>) -> Vec<String> {
    let panic = panic.map(first_line);
    match (cause.map(first_line), panic) {
        (Some(c), Some(p)) if c == p => vec![c.to_string()],
        (Some(c), Some(p)) => vec![format!("red because {c}"), format!("then: {p}")],
        (Some(c), None) => vec![format!("red because {c}")],
        (None, Some(p)) => vec![p.to_string()],
        (None, None) => vec!["panicked (message not captured)".to_string()],
    }
}

/// RAII failure dumper armed for the lifetime of a
/// [`Harness`](super::Harness); `finish` disarms it once its gates passed.
/// Best-effort and panic-safe: it renders under `catch_unwind` and never
/// panics inside `Drop`.
pub(super) struct FailureDump {
    name: String,
    channel: Channel<SignalingNetworkEvent>,
    recorder: Recorder,
    capture: LogCapture,
    judges: Judges,
    views: FailureViews,
    armed: Cell<bool>,
}

impl FailureDump {
    pub(super) fn new(
        name: String,
        channel: Channel<SignalingNetworkEvent>,
        recorder: Recorder,
        judges: Judges,
        views: FailureViews,
    ) -> Self {
        let capture = LogCapture::install();
        Self { name, channel, recorder, capture, judges, views, armed: Cell::new(true) }
    }

    /// Stop the dump from firing — the run reported through its normal path.
    pub(super) fn disarm(&self) {
        self.armed.set(false);
    }

    /// The whole dump, its digest first.
    pub(super) fn render(&self, panic: Option<&str>) -> String {
        let name = &self.name;
        let views: Vec<Rc<dyn FailureView>> = self.views.borrow().clone();
        let cause = views.iter().find_map(|v| v.cause());
        let mut digest = digest(panic, cause.as_deref()).into_iter();
        let mut out = format!("\n══ FAILED '{name}': {} ══\n", digest.next().unwrap_or_default());
        for line in digest {
            out.push_str(&format!("   {line}\n"));
        }
        let mut section = |title: &str, lines: &[String]| {
            out.push_str(&format!("── {title} ──\n"));
            for line in lines {
                out.push_str(&format!("  {line}\n"));
            }
        };
        section("wire ladder", &render_ladder(&self.channel, &self.recorder));
        let events = self.capture.events();
        section("SUT trace per call", &render_call_traces(&events));
        for view in &views {
            for s in view.sections() {
                section(&s.title, &s.lines);
            }
        }
        let mut tables = Vec::new();
        for judge in self.judges.borrow().iter() {
            let summary = judge.summary();
            if summary.is_empty() {
                continue;
            }
            tables.push(format!("{}:", judge.party()));
            tables.extend(summary.into_iter().map(|l| format!("  {l}")));
        }
        if tables.is_empty() {
            tables.push("(no simulated UA keeps a table)".to_string());
        }
        section("UA dialog tables", &tables);
        let (tail, logged) = render_log_tail(&events);
        section(&format!("SUT log ({logged} line(s), tail)"), &tail);
        out.push_str(&format!("══ end of failure dump for '{name}' ══\n"));
        out
    }
}

impl Drop for FailureDump {
    fn drop(&mut self) {
        if !self.armed.get() || !std::thread::panicking() {
            return;
        }
        // Never panic while already unwinding: a second panic in `Drop` aborts
        // the process. Swallow any failure (e.g. a poisoned mutex on snapshot).
        if let Ok(text) = std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.render(super::panic_note::peek_message().as_deref())
        })) {
            emit(text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::digest;

    #[test]
    fn the_panic_is_the_digest_when_no_view_names_a_cause() {
        assert_eq!(
            digest(Some("alice expected 481, got 200 OK\nmore"), None),
            ["alice expected 481, got 200 OK"]
        );
    }

    #[test]
    fn a_views_cause_leads_and_the_panic_follows() {
        assert_eq!(
            digest(Some("bob timed out after 5s waiting for a BYE request"), Some("model x\ncall")),
            ["red because model x", "then: bob timed out after 5s waiting for a BYE request"]
        );
    }

    #[test]
    fn a_cause_the_panic_already_states_is_said_once() {
        assert_eq!(digest(Some("model x\ncall"), Some("model x\ncall")), ["model x"]);
    }
}
