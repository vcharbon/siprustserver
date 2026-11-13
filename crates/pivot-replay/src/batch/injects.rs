//! The batch's inject lines: a cell's `inject` node, due in its run, is asked
//! of the harness as an `inject` line on stdout naming the cell's run-spec, the
//! node, the action and its target; the harness answers on stdin with an
//! `injected` or a `refused` line naming the same run-spec and node, and the
//! cell's run resumes on it. Stdin closing answers every inject still asked,
//! and every later one, refused ([`CLOSED`]).

use std::collections::BTreeMap;
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
}

/// The injects asked and not answered, by `(run-spec, node)`, and whether
/// stdin closed.
#[derive(Default)]
struct Asked {
    pending: BTreeMap<(String, String), oneshot::Sender<Result<(), String>>>,
    closed: bool,
}

/// One answer line on stdin.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
enum AnswerLine {
    /// The harness performed the node's action.
    Injected { spec: String, node: String },
    /// The harness refused or failed it, in its own words.
    Refused { spec: String, node: String, reason: String },
}

impl Injects {
    /// The batch serves its protocol lines on `out`: an inject is asked there.
    pub(super) fn speak_on(&self, out: Out) {
        let _ = self.out.set(out);
    }

    /// The injector of the cell whose run-spec is `spec`, as the cell's line
    /// names it; `None` where the batch serves no protocol lines.
    pub fn injector(self: &Arc<Self>, spec: &str) -> Option<Arc<dyn Injector>> {
        self.out.get()?;
        Some(Arc::new(CellInjector { injects: self.clone(), spec: spec.to_string() }))
    }

    /// Routes the answer `line` to the inject it names; a line that does not
    /// decode, or names no inject asked, is said on stderr.
    pub(super) fn answer(&self, line: &str) {
        let (key, answer) = match serde_json::from_str::<AnswerLine>(line) {
            Ok(AnswerLine::Injected { spec, node }) => ((spec, node), Ok(())),
            Ok(AnswerLine::Refused { spec, node, reason }) => ((spec, node), Err(reason)),
            Err(e) => {
                eprintln!("replay batch: an answer line that does not decode ({e}): {line}");
                return;
            }
        };
        let asked = self.asked.lock().unwrap_or_else(|p| p.into_inner()).pending.remove(&key);
        match asked {
            Some(asked) => {
                let _ = asked.send(answer);
            }
            None => eprintln!("replay batch: an answer for no inject asked: {line}"),
        }
    }

    /// Stdin closed: every inject asked, and every later one, is refused.
    pub(super) fn close(&self) {
        let mut asked = self.asked.lock().unwrap_or_else(|p| p.into_inner());
        asked.closed = true;
        for (_, pending) in std::mem::take(&mut asked.pending) {
            let _ = pending.send(Err(CLOSED.to_string()));
        }
    }

    /// Asks `request` of the harness for the cell whose run-spec is `spec`.
    fn ask(&self, spec: &str, request: InjectRequest) -> Injection {
        let refused = |why: &str| -> Injection {
            let why = why.to_string();
            Box::pin(async move { Err(why) })
        };
        let Some(out) = self.out.get() else {
            return refused("the batch serves no protocol lines to ask an inject on");
        };
        let (tx, rx) = oneshot::channel();
        {
            let mut asked = self.asked.lock().unwrap_or_else(|p| p.into_inner());
            if asked.closed {
                return refused(CLOSED);
            }
            asked.pending.insert((spec.to_string(), request.node.clone()), tx);
        }
        emit(out, &inject_line(spec, &request));
        Box::pin(async move { rx.await.unwrap_or_else(|_| Err(CLOSED.to_string())) })
    }
}

/// The injector of one cell: each inject asked on the batch's lines.
struct CellInjector {
    injects: Arc<Injects>,
    spec: String,
}

impl Injector for CellInjector {
    fn inject(&self, request: InjectRequest) -> Injection {
        self.injects.ask(&self.spec, request)
    }

    fn within(&self) -> Duration {
        INJECT_WITHIN
    }
}
