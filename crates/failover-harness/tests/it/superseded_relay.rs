//! **A non-INVITE relay the dead node made, replicated, and its originator's
//! retransmission at the takeover.** The callee's final to the dead node's
//! relay follows that relay's Via to the dead node and is never failed over
//! (a non-INVITE response is not), and the callee does not repeat it on its
//! own (RFC 3261 §17.2.2): no node can finish that relay. The originator's
//! retransmission reaches the survivor, which holds no server transaction for
//! it, and the relay the record holds for it is superseded: the survivor
//! relays the retransmission afresh and answers it with the final that relay
//! draws. The callee sees the request twice.
//!
//! What the superseded relay must not leave behind: a later UPDATE from the
//! same side is relayed, not refused as overlapping the dead one (RFC 3311
//! §5.2), and the hang-up answers no relay that was already answered.
//!
//! ```text
//!   alice ─REQ─▶ primary ─REQ─▶ bob          (relayed, replicated, held OPEN)
//!               ✗ primary killed ✗   bob's 200 ─╳ (follows the dead node's Via)
//!   alice ─REQ─▶ SURVIVOR ─REQ─▶ bob ─200─▶ SURVIVOR ─200─▶ alice
//! ```

use std::net::SocketAddr;
use std::time::Duration;

use failover_harness::FailoverHarness;
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};

use crate::inflight_resend::{
    assert_not_taken_over, assert_taken_over_once, bring_up, establish, kill_serving_node,
    last_request_sent, reboot_and_reclaim, settle_to_one_cdr, split_mut, survivor_of,
    synchronized_call_ref, KILL_SETTLE, REANSWER, REOFFER, REPLICATE, T1,
};

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// A second offer from the caller, after the round the takeover finished.
const REOFFER_2: &str = "v=0\r\no=alice 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10004 RTP/AVP 0\r\n";
const REANSWER_2: &str = "v=0\r\no=bob 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20004 RTP/AVP 0\r\n";

/// The non-INVITE request's Timer E rung that follows the kill: T1 fell
/// before it, unanswered; the next is 2·T1 later (RFC 3261 §17.1.2.2).
const RESEND_AT: Duration = Duration::from_millis(3 * 500);

/// The responses to a `method` request put on the wire toward `to`.
fn responses_toward(fh: &FailoverHarness, to: SocketAddr, method: &Method) -> Vec<u16> {
    let parser = CustomParser::new();
    fh.sip_entries()
        .into_iter()
        .filter(|e| e.to == to)
        .filter_map(|e| match parser.parse(&e.raw) {
            Ok(SipMessage::Response(r)) if r.cseq().method() == method => Some(r.status()),
            _ => None,
        })
        .collect()
}

/// An UPDATE relayed and replicated, its 200 lost with the dead node: the
/// caller's retransmission is relayed afresh and answered 200, and the
/// caller's next UPDATE is relayed and answered 200 too.
#[tokio::test(start_paused = true)]
async fn a_replicated_update_resent_to_the_takeover_supersedes_its_dead_relay() {
    let mut fh = FailoverHarness::new("superseded-relay-update", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;

    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    // ── Relayed and replicated; the callee holds it open ────────────────────
    let mut update = dialog.send_request(InDialogMethod::Update).with_sdp(REOFFER).send().await;
    let mut first_at_bob = bob.receive("UPDATE").await;
    fh.advance(REPLICATE).await;

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    first_at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    fh.advance(RESEND_AT - REPLICATE - KILL_SETTLE).await;
    assert_not_taken_over(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before, &call_ref);

    // ── The caller's Timer E rung: relayed afresh, answered ─────────────────
    let (wire, dst) = last_request_sent(&fh, alice.addr(), &Method::Update);
    alice.try_send_datagram(&wire, dst).await.expect("the peer's re-send leaves");
    let mut second_at_bob = bob.receive("UPDATE").await;
    second_at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    update.expect(200).await;
    fh.advance(Duration::from_millis(200)).await;
    assert_taken_over_once(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before);

    // ── The caller's next UPDATE is not refused as overlapping ──────────────
    let mut next = dialog.send_request(InDialogMethod::Update).with_sdp(REOFFER_2).send().await;
    bob.receive("UPDATE").await.respond(200, "OK").with_sdp(REANSWER_2).await;
    next.expect(200).await;
    assert_eq!(
        responses_toward(&fh, alice.addr(), &Method::Update),
        [200, 200],
        "one final per UPDATE the caller sent, none from the dead relay",
    );

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// An INFO relayed and replicated, its 200 lost with the dead node: the
/// caller's retransmission is relayed afresh and answered 200; hung up after
/// that transaction left the survivor (Timer J), the call draws no second
/// final for the INFO.
#[tokio::test(start_paused = true)]
async fn a_replicated_info_resent_to_the_takeover_draws_one_final() {
    let mut fh = FailoverHarness::new("superseded-relay-info", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;

    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    let mut info = dialog.send_request(InDialogMethod::Info).send().await;
    let mut first_at_bob = bob.receive("INFO").await;
    fh.advance(REPLICATE).await;

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    first_at_bob.respond(200, "OK").await;
    fh.advance(RESEND_AT - REPLICATE - KILL_SETTLE).await;
    assert_not_taken_over(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before, &call_ref);

    let (wire, dst) = last_request_sent(&fh, alice.addr(), &Method::Info);
    alice.try_send_datagram(&wire, dst).await.expect("the peer's re-send leaves");
    bob.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
    fh.advance(Duration::from_millis(200)).await;
    assert_taken_over_once(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before);

    // ── Past Timer J at the survivor, the call ends ─────────────────────────
    // Timer J is 64·T1 (RFC 3261 §17.2.2).
    fh.advance(64 * T1 + T1).await;
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(
        responses_toward(&fh, alice.addr(), &Method::Info),
        [200],
        "the INFO draws one final, the takeover's; the hang-up answers no relay again",
    );

    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}
