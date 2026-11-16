//! A callee whose ringing provisional the loss model dropped rings again.

use std::time::Duration;

use super::testkit::{caller_spec, scripted_spec};
use crate::actor::*;
use crate::realcall::{CallCtx, CallEnv, LostMessage};
use crate::Harness;

/// Bob rings then stays silent; the call's loss notes report his 180 lost
/// on send, though the fabric delivered it. He rings again, so alice hears a
/// second 180 and CANCELs on it: the call ends `abandoned`. Without the
/// second ring she waits until the step timeout.
#[tokio::test(start_paused = true)]
async fn a_silent_callee_whose_ring_was_lost_rings_again() {
    let h = Harness::new("actor-re-ring");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let env = CallEnv::for_functional(&alice, &bob, None, bob.addr(), "X-Corr", "t");
    let rang_twice =
        |s: &StateInner| s.leg("alice").responses().iter().filter(|f| f.status == 180).count() >= 2;
    let goals = vec![
        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
        Goal::new(Barrier::pred("rang_twice", rang_twice), GoalStep::Cancel { stated: vec![] }),
    ];
    let mut silent = scripted_spec("bob", &bob, vec![]);
    silent.disposition = Disposition::RingThenSilent;
    let call = ActorCall::new(
        vec![caller_spec("alice", &alice, ("bob", bob.clone()), goals), silent],
        vec![],
        SettleBarrier::default_ceiling(),
        Expect::one(Branch::ABANDONED),
    );
    let ctx = CallCtx::new();
    let notes = ctx.loss_notes();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        notes.note(LostMessage {
            leg: Some("bob".to_string()),
            inbound: false,
            method: None,
            status: Some(180),
            cseq_method: "INVITE".to_string(),
            sdp: false,
            call_id: String::new(),
            from_tag: String::new(),
            cseq: 1,
            reliable: false,
            to_tag: String::new(),
        });
    });
    let verdict = run_built_actor_call(call, &env, &ctx).await;
    assert_eq!(verdict.branch(), Some("abandoned"), "{verdict:?}");
    h.finish().await;
}
