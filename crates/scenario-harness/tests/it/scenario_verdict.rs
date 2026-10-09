//! The scenario verdict: a finished actor call is either one of the endings
//! its scenario accepts (`Success { branch }`), or a `Failure` naming where it
//! failed (`FailedAt`: the agent, its pending goal, the pending barrier) and
//! why (`StepError`). Each test drives a full SUT-less actor call (alice
//! straight to bob over the recording simulated network, paused clock) through
//! `run_actor_scenario` / `run_built_actor_call` and asserts the verdict.
//!
//! The context-dependent ending: a refusal of the caller's initial INVITE
//! before the call connects is an accepted ending of a plain call only when
//! the run declares that final as its system under test's overload refusal
//! (`CallEnv::overload_reject`, here `Some(503)`, RFC 3261 §21.5.4) and the
//! call is not an emergency call (RFC 4412 `Resource-Priority`). Undeclared,
//! nothing is accepted.

use scenario_harness::actor::scenarios::{AbandonRinging, BasicCall, InviteReject};
use scenario_harness::actor::{
    phase, run_actor_scenario, run_built_actor_call, ActorScenario, Disposition, FailedAt,
    ScenarioVerdict,
};
use scenario_harness::realcall::{CallCtx, CallEnv};
use scenario_harness::{Harness, StepError};

/// The verdict of one SUT-less run of `scenario`, with `tune` applied to the
/// call env (alice on :5060 calls bob on :5070 directly).
async fn verdict_of(
    name: &str,
    scenario: &dyn ActorScenario,
    tune: impl FnOnce(&mut CallEnv<'_>),
) -> ScenarioVerdict {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let mut env = CallEnv::for_functional(&alice, &bob, None, bob.addr(), "X-Test", name);
    tune(&mut env);
    let verdict = run_actor_scenario(scenario, &env, &CallCtx::new()).await;
    h.finish().await;
    verdict
}

/// The shipped basic call with bob answering its INVITE with `status`
/// instead of ringing then answering.
async fn basic_call_refused_with(
    name: &str,
    status: u16,
    emergency: bool,
    overload_reject: Option<u16>,
) -> ScenarioVerdict {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let mut env = CallEnv::for_functional(&alice, &bob, None, bob.addr(), "X-Test", name);
    env.emergency = emergency;
    env.overload_reject = overload_reject;
    let mut call = BasicCall.build(&env).expect("the basic call builds");
    let bob_spec = call.actors.iter_mut().find(|a| a.role == "bob").expect("bob is declared");
    bob_spec.disposition = Disposition::Reject(status);
    let verdict = run_built_actor_call(call, &env, &CallCtx::new()).await;
    h.finish().await;
    verdict
}

fn success_branch(v: &ScenarioVerdict) -> Option<&'static str> {
    match v {
        ScenarioVerdict::Success { branch } => Some(branch),
        ScenarioVerdict::Failure { .. } => None,
    }
}

#[tokio::test(start_paused = true)]
async fn an_answered_call_is_the_answered_branch() {
    let v = verdict_of("verdict-answered", &BasicCall, |_| {}).await;
    assert_eq!(success_branch(&v), Some("answered"), "got {v:?}");
}

/// The callee's 486 is what `invite_reject` declares: a success, not a
/// `WrongStatus` failure.
#[tokio::test(start_paused = true)]
async fn a_declared_reject_is_its_success_branch() {
    let v = verdict_of("verdict-declared-reject", &InviteReject, |_| {}).await;
    assert_eq!(success_branch(&v), Some("rejected"), "got {v:?}");
}

/// The caller's CANCEL after ringing is what `abandon_ringing` declares.
#[tokio::test(start_paused = true)]
async fn a_declared_abandon_is_its_success_branch() {
    let v = verdict_of("verdict-declared-abandon", &AbandonRinging, |_| {}).await;
    assert_eq!(success_branch(&v), Some("abandoned"), "got {v:?}");
}

#[tokio::test(start_paused = true)]
async fn a_503_before_connect_is_accepted_off_emergency_when_the_run_declares_it() {
    let v = basic_call_refused_with("verdict-503-overload", 503, false, Some(503)).await;
    assert_eq!(success_branch(&v), Some("refused_overload"), "got {v:?}");
}

#[tokio::test(start_paused = true)]
async fn a_503_to_an_emergency_call_is_a_failure_even_when_the_run_declares_it() {
    let v = basic_call_refused_with("verdict-503-emergency", 503, true, Some(503)).await;
    match v {
        ScenarioVerdict::Failure { cause: StepError::WrongStatus { got: 503, .. }, .. } => {}
        other => panic!("an emergency call refused with 503 must fail, got {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_503_is_a_failure_when_the_run_declares_no_overload_refusal() {
    let v = basic_call_refused_with("verdict-503-unexpected", 503, false, None).await;
    match v {
        ScenarioVerdict::Failure { cause: StepError::WrongStatus { got: 503, .. }, .. } => {}
        other => panic!("a 503 without a declared overload refusal must fail, got {other:?}"),
    }
}

/// An undeclared 486 on the basic call fails at alice's pending goal (the BYE,
/// goal #1, still waiting on its guard) while the controller waits on the
/// `established` barrier; the key reads `<who>:<goal kind>#<index>@<barrier>`.
#[tokio::test(start_paused = true)]
async fn a_failed_goal_names_the_agent_its_pending_goal_and_the_barrier() {
    let v = basic_call_refused_with("verdict-failed-goal", 486, false, Some(503)).await;
    let ScenarioVerdict::Failure { at, cause } = v else {
        panic!("an undeclared 486 must fail, got {v:?}");
    };
    assert!(matches!(cause, StepError::WrongStatus { got: 486, .. }), "cause {cause:?}");
    assert_eq!(at, FailedAt { who: "alice", goal: Some(("bye", 1)), barrier: "established" });
    assert_eq!(at.to_string(), "alice:bye#1@established");
}

/// A barrier that never holds fails the call at that barrier, once every
/// actor's goals have fired: no goal is pending.
#[tokio::test(start_paused = true)]
async fn an_unmet_barrier_names_the_barrier() {
    let h = Harness::new("verdict-unmet-barrier");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let env =
        CallEnv::for_functional(&alice, &bob, None, bob.addr(), "X-Test", "verdict-unmet-barrier");
    let mut call = BasicCall.build(&env).expect("the basic call builds");
    call.plan.push(phase("never_holds", |_| false));
    let v = run_built_actor_call(call, &env, &CallCtx::new()).await;
    h.finish().await;
    let ScenarioVerdict::Failure { at, .. } = v else {
        panic!("an unmet barrier must fail, got {v:?}");
    };
    assert_eq!(at.barrier, "never_holds", "at {at}");
    assert_eq!(at.goal, None, "at {at}");
}

/// The callee's 200 crosses the caller's CANCEL in flight (a ring of one hop:
/// the 200 leaves as the CANCEL does). The CANCEL then has no effect
/// (RFC 3261 §9.2): the caller ACKs the 200 and hangs up with a BYE (§15), and
/// `abandon_ringing` ends on its `answered_despite_cancel` branch — torn down,
/// settled and audit-clean.
#[tokio::test(start_paused = true)]
async fn a_200_crossing_the_cancel_is_answered_despite_cancel() {
    let v = verdict_of("verdict-abandon-crossing", &AbandonRinging, |env| {
        env.ring_delay =
            std::time::Duration::from_millis(scenario_harness::SIMULATED_TRANSIT_DELAY_MS);
    })
    .await;
    assert_eq!(success_branch(&v), Some("answered_despite_cancel"), "got {v:?}");
}
