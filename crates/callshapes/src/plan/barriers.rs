//! The barriers a compiled plan gates its goals and phases on: facts the
//! actors observed, each named for the bounded `StepError::who` label.

use scenario_harness::actor::{Barrier, LegPhase, StateInner, SubflowState, SUBFLOW_REALIGN};

/// The renegotiation count of `role` reached `n` — the `reinvited` /
/// `updated` gate.
pub(super) fn renegs_reached(name: &'static str, role: &'static str, n: u32) -> Barrier {
    Barrier::pred(name, move |s| s.leg(role).reneg_count() >= n)
}

/// `role` received `n` 2xx to its `method` requests.
pub(crate) fn answered(
    name: &'static str,
    role: &'static str,
    method: &'static str,
    n: u32,
) -> Barrier {
    Barrier::pred(name, move |s| {
        let ok = |f: &&scenario_harness::actor::ResponseFact| {
            (200..300).contains(&f.status) && f.cseq_method == method
        };
        s.leg(role).responses().iter().filter(ok).count() >= n as usize
    })
}

/// `role` received `n` finals to its `method` requests, whatever their status:
/// a 481 or 408 has ended the dialog by then (RFC 3261 §12.2.1.2), and any
/// other final leaves it to what follows.
pub(super) fn finalled(
    name: &'static str,
    role: &'static str,
    method: &'static str,
    n: u32,
) -> Barrier {
    Barrier::pred(name, move |s| {
        let fin =
            |f: &&scenario_harness::actor::ResponseFact| f.status >= 200 && f.cseq_method == method;
        s.leg(role).responses().iter().filter(fin).count() >= n as usize
    })
}

/// `alice` + `winner` both confirmed — the `established` gate/phase, with the
/// winning callee role captured (bob, or bob2 after a reroute).
pub(crate) fn established_pred(
    winner: &'static str,
) -> impl Fn(&StateInner) -> bool + Send + Sync + Clone {
    move |s: &StateInner| {
        s.leg_at_least("alice", LegPhase::Confirmed) && s.leg_at_least(winner, LegPhase::Confirmed)
    }
}

/// The post-transfer media merge completed on BOTH realign legs.
pub(super) fn merged(s: &StateInner) -> bool {
    let confirmed = |leg: &str| {
        s.leg(leg).subflow(SUBFLOW_REALIGN).is_some_and(|f| f >= SubflowState::Confirmed)
    };
    confirmed("alice") && confirmed("charlie")
}

/// The transfer target declined (its leg terminated without confirming).
pub(super) fn declined(s: &StateInner) -> bool {
    s.leg_at_least("charlie", LegPhase::Terminated)
}
