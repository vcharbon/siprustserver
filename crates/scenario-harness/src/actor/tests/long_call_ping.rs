//! The `long_call` hold follows its one OPTIONS ping: the caller's BYE leaves
//! `long_hold` after the ping's final, so a ping the network lost and the
//! caller's Timer E recovers (RFC 3261 §17.1.2.2) is answered before the BYE
//! ends the dialog, and the callee takes the dialog's requests in CSeq order
//! (§12.2.1.1).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::{Arrival, PreIngressAction};

use crate::actor::scenarios::LongCall;
use crate::actor::*;
use crate::realcall::{CallCtx, CallEnv};
use crate::Harness;

/// The `sent_ms` and start line of the first recorded message `pick` selects.
fn first_sent(
    entries: &[sip_net_testkit::RecordedSipEntry],
    pick: impl Fn(&SipMessage) -> bool,
) -> Option<(u64, String)> {
    entries.iter().find_map(|e| {
        let msg = CustomParser::new().parse(&e.raw).ok()?;
        let line = String::from_utf8_lossy(&e.raw).lines().next().unwrap_or_default().to_string();
        pick(&msg).then_some((e.sent_ms, line))
    })
}

#[tokio::test(start_paused = true)]
async fn the_bye_waits_for_the_answer_to_a_lost_ping() {
    let h = Harness::new("actor-long-call-lost-ping").describe(
        "long_call with a 100 ms hold: the first copy of alice's OPTIONS ping is \
         lost before bob, Timer E resends it at T1, bob answers it, and alice's \
         BYE leaves 100 ms after that 200",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let copies = Arc::new(AtomicUsize::new(0));
    let counter = copies.clone();
    let bob = h
        .agent_with_pre_ingress(
            "bob",
            "127.0.0.1:5070",
            Arc::new(move |arrival: &mut Arrival, _src, _depth| {
                if arrival.raw().starts_with(b"OPTIONS ")
                    && counter.fetch_add(1, Ordering::SeqCst) == 0
                {
                    return PreIngressAction::Drop;
                }
                PreIngressAction::Accept
            }),
        )
        .await;
    let mut env = CallEnv::for_functional(&alice, &bob, None, bob.addr(), "X-Corr", "t");
    env.long_hold = Duration::from_millis(100);

    let verdict = run_actor_scenario(&LongCall, &env, &CallCtx::new()).await;
    assert_eq!(verdict.branch(), Some("answered"), "{verdict:?}");
    assert_eq!(copies.load(Ordering::SeqCst), 2, "the lost ping went again once");

    let entries = h.wire_entries();
    let (answered, final_line) = first_sent(&entries, |m| {
        matches!(m, SipMessage::Response(r) if r.status() >= 200 && r.cseq().method() == "OPTIONS")
    })
    .expect("bob answered the ping");
    assert_eq!(final_line, "SIP/2.0 200 OK", "bob answered the ping on a live dialog");
    let (bye, _) =
        first_sent(&entries, |m| matches!(m, SipMessage::Request(r) if r.method() == "BYE"))
            .expect("alice sent her BYE");
    assert!(
        bye >= answered + 100,
        "the BYE ({bye} ms) left before the hold that follows the ping's 200 ({answered} ms)"
    );
    h.finish().await;
}

/// Whether the caller's BYE goal of `call` opens once both legs are confirmed
/// and her OPTIONS ping drew `status` (`None`: no answer yet).
fn bye_opens(call: &ActorCall, status: Option<u16>) -> bool {
    let obs = ObservedState::new();
    let now = tokio::time::Instant::now();
    obs.record(Observation::LegConfirmed { leg: "alice" }, now);
    obs.record(Observation::LegConfirmed { leg: "bob" }, now);
    if let Some(status) = status {
        let fact = ResponseFact {
            status,
            reason: String::new(),
            cseq_method: "OPTIONS".to_string(),
            body_len: 0,
            body_is_sdp: false,
            early_tag: None,
            typed: None,
        };
        obs.record(Observation::LegResponse { leg: "alice", fact }, now);
    }
    let alice = call.actors.iter().find(|a| a.role == "alice").expect("alice");
    let bye = alice.goals.iter().find(|g| matches!(g.step, GoalStep::Bye)).expect("her BYE");
    obs.with_snapshot(|s| bye.guard.holds(s))
}

/// Any final to the ping opens the hold: a 405 leaves the dialog to the BYE,
/// where waiting for a 2xx would hold the call to the step timeout.
#[tokio::test(start_paused = true)]
async fn any_final_to_the_ping_starts_the_hold() {
    let h = Harness::new("actor-long-call-ping-final");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let env = CallEnv::for_functional(&alice, &bob, None, bob.addr(), "X-Corr", "t");
    let call = LongCall.build(&env).expect("built");
    assert!(!bye_opens(&call, None), "the hold waits for the ping's final");
    assert!(bye_opens(&call, Some(200)));
    assert!(bye_opens(&call, Some(405)), "a non-2xx final opens the hold too");
    h.finish().await;
}
