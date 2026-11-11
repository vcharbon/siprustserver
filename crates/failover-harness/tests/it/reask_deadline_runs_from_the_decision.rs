//! A deadline a service arms when the re-asked decision's attempt starts runs
//! from that decision's answer, not from the re-ask turn's start.
//!
//! ```text
//!   alice ─INVITE─▶ proxy ─▶ primary ──(decision in flight)       backup: the born call
//!                            ✗ primary killed, departure announced ✗
//!   backup asks the decision again; it answers 3 s later; bob dialled, rings
//!   the service's 3 s deadline, armed as the attempt starts, answers alice 480
//!   bob's INVITE CANCELed (RFC 3261 §9.1), 487, ACK; one CDR, nothing left
//! ```

use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua::rules::ServiceDef;
use failover_harness::FailoverHarness;

use crate::departure_support::{bring_up_deciding, kill_announced, placed, CountedDecision};
use crate::inflight_resend::{reboot_and_reclaim, settle_to_one_cdr, split_mut};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// How long each decision round trip takes.
const DECISION_DELAY: Duration = Duration::from_secs(3);

/// The service's deadline, armed as the route's attempt starts.
const DEADLINE_MS: i64 = 3_000;

/// Arms its deadline when an attempt starts and answers the caller 480 when it
/// fires.
mod on_attempt {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceSeed, Terminal, TimerDelay,
    };
    use b2bua::{define_service, sm_rule};
    use call::{TimerKey, TimerType};

    const DEADLINE: TimerType = TimerType::service(ON_ATTEMPT, "deadline");

    define_service! {
        id: "on-attempt",
        machine: ON_ATTEMPT,
        states: OnAttemptState { Idle, Armed },
        init: |_call: &RuleCall| Some(ServiceSeed::new(OnAttemptState::Idle.label())),
        rules: [ on_started(), on_deadline() ],
    }

    fn on_started() -> RuleDefinition {
        sm_rule! {
            id: "on-attempt-arm",
            machine: ON_ATTEMPT,
            active: [ OnAttemptState::Idle ],
            transitions: [ OnAttemptState::Idle => OnAttemptState::Armed ],
            effects: [
                Effect::GuardTimer { timer: DEADLINE, scope: call::TimerScopeKind::Call, label: "arm the deadline" },
            ],
            matcher: Match::attempt_started(),
            handle: |_ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    RuleAction::arm(
                        TimerKey::call(DEADLINE),
                        TimerDelay::millis(super::DEADLINE_MS),
                    ),
                    RuleAction::SetState { machine: ON_ATTEMPT, to: OnAttemptState::Armed.label() },
                ]))
            },
        }
    }

    fn on_deadline() -> RuleDefinition {
        sm_rule! {
            id: "on-attempt-fire",
            machine: ON_ATTEMPT,
            active: [ OnAttemptState::Armed ],
            transitions: [ OnAttemptState::Armed => Terminal ],
            effects: [
                Effect::Respond { status: 480, label: "deadline passed" },
                Effect::LifecycleCommand { label: "terminate the call" },
            ],
            matcher: Match::timer().timer_type(DEADLINE),
            handle: |_ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    RuleAction::RespondToALeg {
                        status: 480,
                        reason: "Temporarily Unavailable".into(),
                        header_updates: vec![],
                        contacts: vec![],
                    },
                    RuleAction::BeginTermination {
                        reason: Some("deadline".into()),
                        cause: call::TerminationCause::Timeout(call::TimeoutKind::NoAnswer),
                        by_leg: None,
                    },
                    RuleAction::ClearState { machine: ON_ATTEMPT },
                ]))
            },
        }
    }
}

fn services() -> Vec<ServiceDef> {
    vec![on_attempt::service_def()]
}

#[tokio::test(start_paused = true)]
async fn a_reasked_calls_attempt_deadline_runs_from_the_decision() {
    let mut fh =
        FailoverHarness::new("reask-deadline", &["b1", "b2"]).with_worker_services(services);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = std::sync::Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, _consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, decision).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    kill_announced(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    let mut uas = bob.receive("INVITE").await;
    let dialled_at = fh.now_ms();
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    call.expect(480).await;
    let released = fh.now_ms() - dialled_at;
    assert!(
        (DEADLINE_MS - 200..DEADLINE_MS + 500).contains(&released),
        "the 480 left {released} ms after bob was dialled; the deadline runs from the decision",
    );

    bob.receive("CANCEL").await.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean("reask-deadline");
    drop(proxy);
}
