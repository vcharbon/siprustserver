//! [`TurnNotes`]: what a turn reports instead of acting on it (ADR-0052 §4):
//! the trace facts of a sampled call, the counts it owes the metrics, and the
//! defects it met. The shell drains them as the turn's core call returns.

use call::Call;

/// Whether this call is traced. The ONE guard every trace fact and emission
/// site uses (ADR-0026).
#[inline]
pub fn sampled(call: &Call) -> bool {
    call.sampled == Some(true)
}

/// One fact of a traced call's story (ADR-0026): its kind, the instant it
/// happened, and what happened in words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceFact {
    pub kind: &'static str,
    pub at_ms: i64,
    pub detail: String,
}

/// An owed PRACK names a leg the call does not hold.
pub const PRACK_HAS_A_LEG: &str = "prack-has-a-leg";
/// An owed PRACK names a dialog its leg does not hold.
pub const PRACK_HAS_A_DIALOG: &str = "prack-has-a-dialog";
/// A machine-bound rule moved its own cursor along an edge it does not
/// declare (ADR-0016 X1).
pub const UNDECLARED_TRANSITION: &str = "rule-declares-its-transitions";
/// A machine-bound rule emitted a side effect it does not declare (ADR-0016 X9).
pub const UNDECLARED_EFFECT: &str = "rule-declares-its-effects";
/// A lifecycle handler took an action outside call ext, its own cursor and
/// its own timers; the action is dropped.
pub const LIFECYCLE_OUT_OF_SCOPE: &str = "lifecycle-handler-keeps-its-scope";

/// Every defect a turn reports, by name.
pub const DEFECTS: [&str; 5] = [
    PRACK_HAS_A_LEG,
    PRACK_HAS_A_DIALOG,
    UNDECLARED_TRANSITION,
    UNDECLARED_EFFECT,
    LIFECYCLE_OUT_OF_SCOPE,
];

/// A defect a turn met mid-way: a step that found the record unable to carry
/// what it must do, or a rule acting outside what it declared. The call is the
/// record the step read.
#[derive(Debug, Clone)]
pub struct Defect {
    pub name: &'static str,
    pub detail: String,
    pub call: Box<Call>,
}

/// What a turn has reported and the shell has not drained yet.
#[derive(Debug, Default)]
pub struct TurnNotes {
    trace: Vec<TraceFact>,
    repeat_give_ups: Vec<&'static str>,
    message_caps: u32,
    defects: Vec<Defect>,
}

impl TurnNotes {
    /// A trace fact of `call` of `kind` at `at_ms`, `detail` rendered only for
    /// a sampled call: an unsampled call builds nothing.
    pub fn trace(
        &mut self,
        call: &Call,
        kind: &'static str,
        at_ms: i64,
        detail: impl FnOnce() -> String,
    ) {
        if sampled(call) {
            self.trace.push(TraceFact { kind, at_ms, detail: detail() });
        }
    }

    /// A dialog-level ladder of obligation `kind` ran to its give-up.
    pub fn repeat_give_up(&mut self, kind: &'static str) {
        self.repeat_give_ups.push(kind);
    }

    /// The message cap tore a runaway call down.
    pub fn message_cap_terminated(&mut self) {
        self.message_caps = self.message_caps.saturating_add(1);
    }

    /// The defect `name`, with `detail`, met on `call`.
    pub fn defect(&mut self, name: &'static str, detail: String, call: &Call) {
        self.defects.push(Defect { name, detail, call: Box::new(call.clone()) });
    }

    /// Everything reported so far, leaving nothing behind.
    pub fn take(&mut self) -> TurnNotes {
        std::mem::take(self)
    }

    /// The trace facts, in the order the turn met them.
    pub fn trace_facts(&self) -> &[TraceFact] {
        &self.trace
    }

    /// The obligation kinds of the ladders that gave up, one per give-up.
    pub fn repeat_give_ups(&self) -> &[&'static str] {
        &self.repeat_give_ups
    }

    /// How many runaway calls the message cap tore down.
    pub fn message_caps(&self) -> u32 {
        self.message_caps
    }

    /// The defects, in the order the turn met them.
    pub fn defects(&self) -> &[Defect] {
        &self.defects
    }
}
