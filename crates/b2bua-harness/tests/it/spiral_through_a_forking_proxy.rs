//! A spiral (RFC 3261 §16.3) through a third-party proxy that forks (§16.6):
//! call 1's outgoing INVITE is forwarded in parallel back to the B2BUA, where
//! it becomes call 2 toward bob, and to carol.
//!
//!   alice ──▶ b2bua (call 1) ──▶ proxy ─┬─▶ b2bua (call 2) ──▶ bob
//!                                       └─▶ carol
//!
//! Both branches ring, so call 1 holds two early dialogs on its one outgoing
//! leg and alice sees both. One branch answers; the proxy forwards that 2xx
//! and CANCELs the other branch (§16.7 step 10), whose 487 it ACKs and
//! absorbs. When the spiral loses, call 2 is CANCELed by the proxy's CANCEL
//! on its branch, which carries the Call-ID and From tag of call 1's outgoing
//! leg; when it wins, carol is CANCELed and call 2 carries the dialog.

use b2bua_harness::settle_until;
use call::CdrEventType;
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};

use crate::common::spiral::{
    assert_both_answered_and_ended, assert_two_calls, forking_spiral_scene, kinds, CAROL_PORT,
};
use crate::common::stateful_proxy::RecordRoute;

const CAROL_ANSWER: &str = "v=0\r\no=carol 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";

/// Which branch of the fork answers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Winner {
    /// The branch back to the B2BUA: bob answers through call 2.
    Spiral,
    /// The branch to carol.
    Carol,
}

async fn fork_answered(name: &str, record_route: RecordRoute, winner: Winner) {
    let (s, _proxy) = forking_spiral_scene(name, record_route).await;
    let carol = s.h.agent("carol", &format!("127.0.0.1:{CAROL_PORT}")).await;

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut bob = s.bob.receive("INVITE").await;
    let mut at_carol = carol.receive("INVITE").await;

    bob.respond(180, "Ringing").await;
    let through_spiral = call.expect(180).await;
    at_carol.respond(180, "Ringing").await;
    let from_carol = call.expect(180).await;
    assert_ne!(
        through_spiral.to().tag(),
        from_carol.to().tag(),
        "alice sees the two early dialogs as two"
    );

    let mut dialog = match winner {
        Winner::Spiral => {
            bob.respond(200, "OK").with_sdp(ANSWER_SDP).await;
            let mut cancel = carol.receive("CANCEL").await;
            cancel.respond(200, "OK").await;
            at_carol.respond(487, "Request Terminated").await;
            at_carol.expect_ack().await;
            let ok = call.expect(200).await;
            assert_eq!(ok.to().tag(), through_spiral.to().tag(), "the spiral's early dialog wins");
            let dialog = call.ack().await;
            s.bob.receive("ACK").await;
            dialog
        }
        Winner::Carol => {
            at_carol.respond(200, "OK").with_sdp(CAROL_ANSWER).await;
            let mut cancel =
                s.bob.try_receive("CANCEL").await.expect("the proxy's CANCEL reaches bob");
            cancel.respond(200, "OK").await;
            bob.respond(487, "Request Terminated").await;
            bob.expect_ack().await;
            let ok = call.expect(200).await;
            assert_eq!(ok.to().tag(), from_carol.to().tag(), "carol's early dialog wins");
            let dialog = call.ack().await;
            carol.receive("ACK").await;
            dialog
        }
    };

    let answerer = if winner == Winner::Spiral { &s.bob } else { &carol };
    let mut bye = dialog.bye().await;
    answerer.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| s.b2bua.cdr_records().len() == 2 && s.b2bua.is_reaped()).await;
    match winner {
        Winner::Spiral => assert_both_answered_and_ended(&s, &call.call_id()),
        Winner::Carol => {
            let (first, second) = assert_two_calls(&s, &call.call_id());
            let k = kinds(&first);
            assert!(k.contains(&CdrEventType::Answer) && k.contains(&CdrEventType::Bye), "{k:?}");
            let k = kinds(&second);
            assert!(
                k.contains(&CdrEventType::Cancel)
                    && !k.contains(&CdrEventType::Answer)
                    && !k.contains(&CdrEventType::Bye),
                "call 2 ends CANCELed, never answered: {k:?}"
            );
        }
    }
    drop(carol);
    let _ = s.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_forked_spiral_answered_through_the_spiral() {
    fork_answered("spiral-fork-spiral-wins", RecordRoute::Yes, Winner::Spiral).await;
}

#[tokio::test(start_paused = true)]
async fn a_forked_spiral_answered_by_the_other_branch() {
    fork_answered("spiral-fork-spiral-loses", RecordRoute::Yes, Winner::Carol).await;
}

#[tokio::test(start_paused = true)]
async fn a_forked_spiral_without_record_route_answered_through_the_spiral() {
    fork_answered("spiral-fork-no-rr-spiral-wins", RecordRoute::No, Winner::Spiral).await;
}

#[tokio::test(start_paused = true)]
async fn a_forked_spiral_without_record_route_answered_by_the_other_branch() {
    fork_answered("spiral-fork-no-rr-spiral-loses", RecordRoute::No, Winner::Carol).await;
}
