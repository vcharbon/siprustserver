//! The batch's inject lines: a cell's `inject` node, due in its run, is asked
//! of the harness as an `inject` line on stdout naming the batch's own id of
//! the ask, the cell's run-spec, the node, the action and its target; the
//! harness answers on stdin with an `injected` or a `refused` line echoing the
//! same id, run-spec and node, and the cell's run resumes on it. Each ask has
//! an id of its own, so an answer meant for an earlier ask never resolves a
//! later one of the same run-spec and node. Stdin closing answers every inject
//! still asked, and every later one, refused ([`CLOSED`]). An inject stays
//! asked only while the run that asked it lasts: an answer after that run
//! ended routes nowhere.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pivot_interpreter::{InjectRequest, Injection, Injector};
use serde::Deserialize;
use tokio::sync::oneshot;

use super::{emit, inject_line, Out};

/// How long a cell waits for the harness to answer an inject line: above an
/// operation's default `inject_s` (ADR-0060 appendix §7), so the harness's own
/// timeout answers first, and under the run's wall margin.
pub const INJECT_WITHIN: Duration = Duration::from_secs(90);

/// Why an inject is refused once stdin closed.
pub const CLOSED: &str = "the harness closed the batch's stdin before it answered";

/// The inject lines of one batch: where they are written, and each one asked
/// and not answered yet.
#[derive(Default)]
pub struct Injects {
    /// Where the protocol lines go, once the batch serves them.
    out: OnceLock<Out>,
    asked: Mutex<Asked>,
    /// The last run handed an injector, each run's own number.
    runs: AtomicU64,
    /// The last ask's id, each ask's own number.
    asks: AtomicU64,
}

/// The injects asked and not answered, by their ask id, and whether stdin
/// closed.
#[derive(Default)]
struct Asked {
    pending: BTreeMap<u64, Pending>,
    closed: bool,
}

/// One inject asked: the run-spec and node it is of, the run that asked it,
/// and where its answer goes.
struct Pending {
    spec: String,
    node: String,
    run: u64,
    answer: oneshot::Sender<Result<(), String>>,
}

/// One answer line on stdin.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
enum AnswerLine {
    /// The harness performed the node's action.
    Injected { ask: String, spec: String, node: String },
    /// The harness refused or failed it, in its own words.
    Refused { ask: String, spec: String, node: String, reason: String },
}

impl Injects {
    /// The batch serves its protocol lines on `out`: an inject is asked there.
    pub(super) fn speak_on(&self, out: Out) {
        let _ = self.out.set(out);
    }

    /// The injector of one run of the cell whose run-spec is `spec`, as the
    /// cell's line names it; `None` where the batch serves no protocol lines.
    /// Dropping it, as the run ends, withdraws every inject it still asks.
    pub fn injector(self: &Arc<Self>, spec: &str) -> Option<Arc<dyn Injector>> {
        self.out.get()?;
        let run = self.runs.fetch_add(1, Ordering::SeqCst) + 1;
        Some(Arc::new(CellInjector { injects: self.clone(), spec: spec.to_string(), run }))
    }

    /// Every inject asked and not answered, as `(run-spec, node)`, in the
    /// order they were asked.
    pub(super) fn asked(&self) -> Vec<(String, String)> {
        let asked = self.asked.lock().unwrap_or_else(|p| p.into_inner());
        asked.pending.values().map(|pending| (pending.spec.clone(), pending.node.clone())).collect()
    }

    /// What the batch awaits of the harness's answers, for its waiting line:
    /// each inject asked and not answered, its run-spec by file name.
    pub(super) fn awaited(&self) -> String {
        let asked = self.asked();
        if asked.is_empty() {
            return "no inject asked".to_string();
        }
        let named: Vec<String> = asked
            .iter()
            .map(|(spec, node)| {
                let file = Path::new(spec).file_name().and_then(|n| n.to_str()).unwrap_or(spec);
                format!("{file} {node}")
            })
            .collect();
        format!("{} inject(s) asked: {}", asked.len(), named.join(", "))
    }

    /// Routes the answer `line` to the inject whose id, run-spec and node it
    /// echoes; a line that does not decode, or echoes no inject asked, is said
    /// on stderr.
    pub(super) fn answer(&self, line: &str) {
        let (ask, spec, node, answer) = match serde_json::from_str::<AnswerLine>(line) {
            Ok(AnswerLine::Injected { ask, spec, node }) => (ask, spec, node, Ok(())),
            Ok(AnswerLine::Refused { ask, spec, node, reason }) => (ask, spec, node, Err(reason)),
            Err(e) => {
                eprintln!("replay batch: an answer line that does not decode ({e}): {line}");
                return;
            }
        };
        let asked = {
            let mut asked = self.asked.lock().unwrap_or_else(|p| p.into_inner());
            let id = ask.parse::<u64>().ok().filter(|id| {
                asked
                    .pending
                    .get(id)
                    .is_some_and(|pending| pending.spec == spec && pending.node == node)
            });
            id.and_then(|id| asked.pending.remove(&id))
        };
        match asked {
            Some(asked) => {
                let _ = asked.answer.send(answer);
            }
            None => eprintln!("replay batch: an answer for no inject asked: {line}"),
        }
    }

    /// Stdin closed: every inject asked, and every later one, is refused.
    pub(super) fn close(&self) {
        let mut asked = self.asked.lock().unwrap_or_else(|p| p.into_inner());
        asked.closed = true;
        for (_, pending) in std::mem::take(&mut asked.pending) {
            let _ = pending.answer.send(Err(CLOSED.to_string()));
        }
    }

    /// The run numbered `run` ended: the injects it still asks are withdrawn.
    fn withdraw(&self, run: u64) {
        let mut asked = self.asked.lock().unwrap_or_else(|p| p.into_inner());
        asked.pending.retain(|_, pending| pending.run != run);
    }

    /// Asks `request` of the harness for the run numbered `run` of the cell
    /// whose run-spec is `spec`, under an ask id of its own.
    fn ask(&self, spec: &str, run: u64, request: InjectRequest) -> Injection {
        let refused = |why: &str| -> Injection {
            let why = why.to_string();
            Box::pin(async move { Err(why) })
        };
        let Some(out) = self.out.get() else {
            return refused("the batch serves no protocol lines to ask an inject on");
        };
        let (tx, rx) = oneshot::channel();
        let id = {
            let mut asked = self.asked.lock().unwrap_or_else(|p| p.into_inner());
            if asked.closed {
                return refused(CLOSED);
            }
            let id = self.asks.fetch_add(1, Ordering::SeqCst) + 1;
            let pending =
                Pending { spec: spec.to_string(), node: request.node.clone(), run, answer: tx };
            asked.pending.insert(id, pending);
            id
        };
        emit(out, &inject_line(&id.to_string(), spec, &request));
        Box::pin(async move { rx.await.unwrap_or_else(|_| Err(CLOSED.to_string())) })
    }
}

/// The injector of one run of a cell: each inject asked on the batch's lines,
/// withdrawn when the run drops it.
struct CellInjector {
    injects: Arc<Injects>,
    spec: String,
    run: u64,
}

impl Injector for CellInjector {
    fn inject(&self, request: InjectRequest) -> Injection {
        self.injects.ask(&self.spec, self.run, request)
    }

    fn within(&self) -> Duration {
        INJECT_WITHIN
    }
}

impl Drop for CellInjector {
    fn drop(&mut self) {
        self.injects.withdraw(self.run);
    }
}

#[cfg(test)]
mod tests;
