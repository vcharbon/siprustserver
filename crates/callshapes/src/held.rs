//! The compile of an [`Establishment::Held`] call: the callee that rings at
//! once and answers when the early stages completed ([`held_callee`],
//! [`place_answer`]), the early stages themselves ([`compile_early`]), and
//! the CANCEL teardown timed against that callee ([`compile_cancel`]).

use std::time::Duration;

use scenario_harness::actor::{
    phase, ActorSpec, Barrier, Branch, CtxFeed, Disposition, Expect, Goal, GoalStep, LegPhase,
    MediaState,
};
use scenario_harness::realcall::CallEnv;
use scenario_harness::{Agent, ANSWER_SDP};
use sip_message::generators::InDialogMethod;

use crate::plan::{
    answered, established_pred, Build, CancelAt, EarlyMethod, Establishment, Forks, Party, TRANSIT,
};

/// The held callee `bob`: it rings after `ring_after`; its answer is placed
/// later, by [`place_answer`] or [`compile_cancel`].
pub(crate) fn held_callee(
    agent: Agent,
    forks: Forks,
    reliable: bool,
    ring_after: Duration,
    feed: CtxFeed,
) -> ActorSpec {
    let tags = match forks {
        Forks::Sole => &[][..],
        Forks::Forked { tags, .. } => tags,
    };
    ActorSpec {
        role: "bob",
        agent,
        disposition: Disposition::Held,
        media: MediaState::answer(ANSWER_SDP),
        goals: vec![
            Goal::new(Barrier::None, GoalStep::Ring { forks: tags, reliable }).after(ring_after)
        ],
        invite_targets: vec![],
        via: None,
        feed,

        cseq: None,
        delayed: vec![],
        claim: None,
    }
}

/// The gate the first early request, else the answer, waits on: the caller
/// has every reliable provisional's PRACK answered, else she rang.
pub(crate) fn early_gate(forks: Forks, reliable: bool) -> Barrier {
    if reliable {
        let forks = u32::try_from(forks.count()).unwrap_or(u32::MAX);
        answered("early", "alice", "PRACK", forks)
    } else {
        Barrier::pred("ringing", |s| s.leg_rang("alice"))
    }
}

/// `count` early requests `by` sends one after another, each on the previous
/// one's 2xx; the gate becomes "all answered".
pub(crate) fn compile_early(
    establish: Establishment,
    b: &mut Build,
    by: Party,
    method: EarlyMethod,
    count: u32,
) {
    let role = b.role(by);
    let fork = match (by, establish) {
        (Party::Callee, Establishment::Held { forks, .. }) => forks.early_fork(),
        _ => None,
    };
    let (method, name) = match method {
        EarlyMethod::Update => (InDialogMethod::Update, "UPDATE"),
        EarlyMethod::Info => (InDialogMethod::Info, "INFO"),
    };
    let sent = |g: &Goal| matches!(g.step, GoalStep::EarlyRequest { method: m, .. } if m == method);
    let base =
        u32::try_from(b.goals_mut(role).iter().filter(|g| sent(g)).count()).unwrap_or(u32::MAX);
    for i in 0..count {
        let guard =
            if i == 0 { b.gate.clone() } else { answered("early_exchanged", role, name, base + i) };
        b.goals_mut(role).push(Goal::new(guard, GoalStep::EarlyRequest { method, fork }));
    }
    if method == InDialogMethod::Update {
        b.add_renegs(role, count);
    }
    b.gate = answered("early_exchanged", role, name, base + count);
}

/// The held callee's answer, once the early stages completed and the ring
/// dwell elapsed; the gate becomes `established`. A no-op when no held answer
/// is pending.
pub(crate) fn place_answer(env: &CallEnv<'_>, b: &mut Build) {
    let Some(winner) = b.held_answer.take() else { return };
    let gate = b.gate.clone();
    b.callee_mut(b.winner)
        .goals
        .push(Goal::new(gate, GoalStep::Answer { winner }).after(env.ring_delay));
    let est = established_pred(b.winner);
    b.phases.push(phase("established", est.clone()));
    b.gate = Barrier::pred("established", est);
}

/// How long the held callee waits before it rings, under a CANCEL at `at`:
/// past the CANCEL that must find it not ringing yet.
pub(crate) fn ring_after(at: CancelAt) -> Duration {
    match at {
        CancelAt::BeforeProvisional => TRANSIT * 20,
        _ => Duration::ZERO,
    }
}

/// The caller's CANCEL at `at`, then her BYE should an answer cross it; the
/// held callee answers on the same gate the CANCEL is timed from.
///
/// From the gate G, the callee's 2xx leaves at G + ring and the SUT relays it
/// at G + ring + T (T: [`TRANSIT`]); a CANCEL leaving the caller at G + d
/// reaches the SUT at G + d + T and the callee at G + d + 2T.
pub(crate) fn compile_cancel(env: &CallEnv<'_>, b: &mut Build, at: CancelAt) {
    let ring = env.ring_delay;
    let trying = Barrier::pred("trying", |s| s.leg("alice").saw_status(100));
    // The SUT's 100 Trying reaches the caller at 2T, the callee's relayed
    // provisional (rung at 2T) at 4T.
    let (guard, dwell, expect) = match at {
        CancelAt::BeforeProvisional => (trying, TRANSIT, Expect::one(Branch::ABANDONED)),
        CancelAt::CrossingProvisional => (trying, TRANSIT * 3 / 2, Expect::one(Branch::ABANDONED)),
        CancelAt::AfterProvisional => (b.gate.clone(), TRANSIT * 3, Expect::one(Branch::ABANDONED)),
        CancelAt::CrossingFinal => {
            const ENDINGS: &[Branch] = &[Branch::ABANDONED, Branch::ANSWERED_DESPITE_CANCEL];
            (b.gate.clone(), ring - TRANSIT * 3 / 2, Expect::of(ENDINGS))
        }
        CancelAt::AfterFinal => {
            (b.gate.clone(), ring + TRANSIT / 2, Expect::one(Branch::ANSWERED_DESPITE_CANCEL))
        }
    };
    b.caller_goals.push(Goal::new(guard, GoalStep::Cancel { stated: Vec::new() }).after(dwell));
    // An answer that crossed the CANCEL confirmed the call: hang up (RFC 3261
    // §15). After a 487 the accepted ending drops this goal.
    let answered_or_ended =
        |s: &scenario_harness::actor::StateInner| s.leg_at_least("alice", LegPhase::Confirmed);
    b.caller_goals
        .push(Goal::new(Barrier::pred("answered", answered_or_ended), GoalStep::ByeIfConfirmed));
    if let Some(winner) = b.held_answer.take() {
        let gate = b.gate.clone();
        b.callee_mut(b.winner).goals.push(Goal::new(gate, GoalStep::Answer { winner }).after(ring));
    }
    b.expect = expect;
}
