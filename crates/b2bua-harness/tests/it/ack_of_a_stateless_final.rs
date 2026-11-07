//! An ACK above the dialog's last CSeq that no INVITE at this node explains:
//! the ACK a peer owes a final some hop or this node's parser gave statelessly
//! to its re-INVITE (RFC 3261 §17.1.1.3). §14.1 leaves the session unchanged, so
//! on a call no other owner served the ACK is ignored (ADR-0055 §4).

use std::time::Duration;

use b2bua_harness::{B2buaScene, B2buaSut};
use scenario_harness::WaiverScope;
use sip_message::generators::InDialogMethod;
use sip_message::SipMessage;

/// The first in-dialog CSeq above the harness caller's INVITE's 1001.
const ABOVE_THE_LAST: u32 = 1002;

const BOB_ANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30002 RTP/AVP 0\r\n";
const OFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";

fn named(msg: Option<SipMessage>) -> Option<(u32, String)> {
    msg.map(|m| (m.cseq().seq(), m.cseq().method().as_str().to_string()))
}

/// A scene whose keepalive stays out of the windows these tests leave idle.
async fn quiet_scene(name: &str) -> B2buaScene {
    B2buaScene::with_b2bua(name, |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| c.keepalive_interval_sec = 3_600)
    })
    .await
}

async fn assert_call_stays_up_then_hang_up(s: &B2buaScene, dialog: &mut scenario_harness::Dialog) {
    s.h.advance(Duration::from_secs(2)).await;
    let at_bob = named(s.bob.take_queued().await);
    assert!(at_bob.is_none(), "bob hears nothing (RFC 3261 §14.1), got {at_bob:?}");
    let at_alice = named(s.alice.take_queued().await);
    assert!(at_alice.is_none(), "alice's session is unchanged, got {at_alice:?}");
    s.hangup(dialog).await;
}

/// The worker alone: an in-dialog ACK of CSeq 1002 under the dialog's tags, no
/// INVITE of that CSeq ever having reached it.
#[tokio::test(start_paused = true)]
async fn an_ack_above_the_last_cseq_with_no_invite_leaves_the_call_up() {
    let s = B2buaScene::new("ack-above-the-last-cseq").await;
    s.h.waive(
        WaiverScope::rule(
            "ack-cseq-matches-invite",
            "the INVITE of this ACK was refused by a hop this rig does not hold",
        )
        .on_party("alice"),
    );
    let mut dialog = s.establish().await;
    dialog.ack_for(ABOVE_THE_LAST, None).await;
    assert_call_stays_up_then_hang_up(&s, &mut dialog).await;
    assert_eq!(s.b2bua.metrics().unexplained_acks_total(), 1, "the ACK it ignored is counted");
    s.finish().await;
}

/// The worker's own parser refuses a To-tagged INVITE (a peer reaching it
/// directly): its 400 keeps the dialog's tag and her ACK comes back to it.
#[tokio::test(start_paused = true)]
async fn the_ack_of_the_workers_parse_refusal_of_a_reinvite_leaves_the_call_up() {
    let s = B2buaScene::new("worker-parse-refused-reinvite").await;
    let mut dialog = s.establish().await;
    let remote_tag = dialog.remote_tag().to_string();
    let mut round = dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(OFFER)
        .with_header("Expires", "soon")
        .send()
        .await;
    let refused = round.expect(400).await;
    assert_eq!(refused.to().tag(), Some(remote_tag.as_str()), "the 400 keeps the dialog's tag");
    assert_call_stays_up_then_hang_up(&s, &mut dialog).await;
    assert_eq!(s.b2bua.metrics().unexplained_acks_total(), 1, "the ACK it ignored is counted");
    s.finish().await;
}

/// Control: the re-INVITE reaches the worker and the callee refuses it. The
/// worker's server transaction takes her ACK.
#[tokio::test(start_paused = true)]
async fn the_ack_of_a_relayed_refusal_leaves_the_call_up() {
    let s = quiet_scene("relayed-refusal-of-a-reinvite").await;
    let mut dialog = s.establish().await;
    let mut round = dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut at_bob = s.bob.receive("INVITE").await;
    at_bob.respond(488, "Not Acceptable Here").await;
    let refused = round.expect(488).await;
    s.bob.take_queued().await;
    // A copy of her ACK past Timer I, when no transaction is left to take it.
    s.h.advance(Duration::from_secs(40)).await;
    round.ack_non_2xx(&refused).await.expect("the ACK's copy leaves");
    s.h.advance(Duration::from_secs(1)).await;
    s.bob.take_queued().await;
    assert_call_stays_up_then_hang_up(&s, &mut dialog).await;
    assert_eq!(s.b2bua.metrics().unexplained_acks_total(), 0, "this ACK is explained");
    s.finish().await;
}

/// Control: the ACK of an accepted re-INVITE repeated after the worker's
/// transaction is gone is at the dialog's last CSeq, so it is no evidence.
#[tokio::test(start_paused = true)]
async fn a_late_copy_of_an_accepted_reinvites_ack_leaves_the_call_up() {
    let s = quiet_scene("late-copy-of-an-ack").await;
    let mut dialog = s.establish().await;
    let mut round = dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut at_bob = s.bob.receive("INVITE").await;
    at_bob.respond(200, "OK").with_sdp(BOB_ANSWER).await;
    round.expect(200).await;
    dialog.ack(None).await;
    s.bob.receive("ACK").await;
    s.h.advance(Duration::from_secs(40)).await;
    dialog.ack(None).await;
    s.h.advance(Duration::from_secs(2)).await;
    let at_alice = named(s.alice.take_queued().await);
    assert!(at_alice.is_none(), "alice's session is unchanged, got {at_alice:?}");
    let _copy_at_bob = s.bob.take_queued().await;
    let at_bob = named(s.bob.take_queued().await);
    assert!(at_bob.is_none(), "no BYE reaches bob, got {at_bob:?}");
    s.hangup(&mut dialog).await;
    s.finish().await;
}
