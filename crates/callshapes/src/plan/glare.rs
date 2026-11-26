//! [`Stage::Glare`](super::Stage::Glare): both parties renegotiate the current
//! dialog at once.
//!
//! Each party sends one offer (a re-INVITE or an UPDATE) on the same gate
//! after the same dwell, so the two offers cross wherever they meet: an agent
//! that takes an offer while its own is outstanding answers `491`, and each
//! refused party retries after its §14.1 dwell, the Call-ID's owner waiting
//! longer (RFC 3261 §14.1, RFC 3311 §5.2). The stage ends once both parties
//! completed their renegotiation, retried or refused.

use scenario_harness::actor::Barrier;
use scenario_harness::realcall::CallEnv;

use super::{Build, Party, Script, ShapePlan};

/// Whether `script` is one renegotiation a glare stage can cross.
pub(super) fn crossable(script: Script) -> bool {
    matches!(script, Script::Reinvite { n: 1 } | Script::UpdatePostConnect)
}

impl ShapePlan {
    /// Compile the caller's and the callee's offer on the one incoming gate;
    /// the next gate is both completions.
    pub(super) fn compile_glare(
        &self,
        env: &CallEnv<'_>,
        b: &mut Build,
        caller: Script,
        callee: Script,
    ) {
        let start = b.gate.clone();
        self.compile_script(env, b, &caller, Party::Caller);
        let caller_done = std::mem::replace(&mut b.gate, start);
        self.compile_script(env, b, &callee, Party::Callee);
        let callee_done = b.gate.clone();
        b.gate = Barrier::pred("glare_done", move |s| caller_done.holds(s) && callee_done.holds(s));
    }
}
