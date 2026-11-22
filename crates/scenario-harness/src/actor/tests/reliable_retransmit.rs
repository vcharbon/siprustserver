//! A reliable provisional is passed to the transport again from T1, doubling,
//! until its PRACK arrives (RFC 3262 §3): one the network loses reaches the
//! caller on its retransmission, and the call goes on.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sip_net_testkit::Fate;

use super::testkit::caller_spec;
use crate::actor::*;
use crate::realcall::InvitePlan;
use crate::{Agent, Harness, ANSWER_SDP};

/// The datagrams `agent` received whose start line begins `start`.
fn received(agent: &Agent, start: &str) -> usize {
    agent.wire_view().iter().filter(|e| e.raw.starts_with(start.as_bytes())).count()
}

/// The fabric loses the first `183` it carries; every other datagram arrives.
fn lose_first_183() -> sip_net_testkit::SendFate {
    let lost = AtomicBool::new(false);
    Arc::new(move |_src, _dst, raw| {
        if raw.starts_with(b"SIP/2.0 183") && !lost.swap(true, Ordering::SeqCst) {
            Fate::Lose
        } else {
            Fate::Deliver
        }
    })
}

#[tokio::test(start_paused = true)]
async fn a_lost_reliable_183_is_retransmitted_and_the_call_goes_on() {
    let h =
        Harness::with_transit_delay_and_send_fate("actor-reliable-183-lost", 1, lose_first_183())
            .describe(
                "RFC 3262 §3: bob's reliable 183 is lost; bob passes it to the transport \
             again at T1, alice PRACKs it, bob answers 200 and alice hangs up",
            );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let plan = InvitePlan {
        via: bob.addr(),
        from: None,
        to: None,
        ruri: None,
        call_id: None,
        headers: vec![("Supported".to_string(), "100rel".to_string())],
        rewrite: Default::default(),
    };
    let caller = caller_spec(
        "alice",
        &alice,
        ("bob", bob.clone()),
        vec![
            Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: Some(plan) }),
            Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye),
        ],
    );
    let ring = Provisional {
        fork: None,
        status: 183,
        sdp: true,
        reliable: true,
        prack_answer_after: Duration::ZERO,
    };
    let callee = ActorSpec {
        role: "bob",
        agent: bob.clone(),
        disposition: Disposition::Held,
        media: MediaState::answer(ANSWER_SDP),
        goals: vec![
            Goal::new(Barrier::None, GoalStep::Provisional(ring)),
            Goal::new(Barrier::None, GoalStep::Final(HeldFinal { status: 200, winner: None })),
        ],
        invite_targets: vec![],
        via: None,
        feed: CtxFeed::default(),
        cseq: None,
        delayed: vec![],
        answers: Default::default(),
        claim: None,
    };
    let call = CallPlan {
        actors: vec![caller, callee],
        plan: vec![super::testkit::established_phase()],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the call completes, got {verdict:?}");
    assert_eq!(received(&alice, "SIP/2.0 100"), 1, "bob's 100 stops alice's Timer A");
    assert_eq!(received(&bob, "INVITE "), 1, "no INVITE copy draws the 183 again");
    assert_eq!(received(&alice, "SIP/2.0 183"), 1, "the retransmission, the first one lost");
    assert_eq!(received(&bob, "PRACK "), 1, "alice PRACKs the 183 she took");
    assert_eq!(received(&alice, "SIP/2.0 504"), 0, "bob never gives up");
    h.finish().await;
}
