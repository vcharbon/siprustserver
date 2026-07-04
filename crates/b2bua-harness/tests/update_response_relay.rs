//! In-dialog UPDATE transaction-handling completeness (GAP-P8b-5 + GAP-P8b-2).
//!
//! 1. **Non-2xx finals of relayed non-INVITEs are relayed back** (GAP-P8b-5,
//!    `relay-non-invite-failure`): the B2BUA relays an UPDATE (or INFO) onto
//!    the peer dialog; the far end's 481/488 final must reach the requester —
//!    plain transaction-layer symmetry (RFC 3261 §8.1.3.3), the non-INVITE
//!    sibling of `relay-reinvite-response`. Like a failed re-INVITE (§14.1),
//!    the failure is only *reported*: the dialog and the call stay up (a 481
//!    to an UPDATE does not tear the call down by itself; a truly dead dialog
//!    is still reaped by the keepalive-OPTIONS 481 → `handle-481` path, which
//!    is unaffected — B2BUA-originated requests leave no pending-relay
//!    snapshot).
//! 2. **UPDATE during a pending failover reroute gets a local 491**
//!    (GAP-P8b-2, `update-peer-unavailable`): the b-leg failed, `/call/failure`
//!    produced a reroute, the replacement leg is not usable yet — relaying the
//!    UPDATE would fire into a dead dialog (or be silently dropped against a
//!    tag-less one), so the B2BUA answers 491 Request Pending locally and the
//!    requester retries once the reroute settles.
//!
//! The 2xx-relay happy paths (early + confirmed UPDATE, INFO) are pinned by
//! `prack_update_forking.rs`; these tests cover the failure halves.

use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallTreatment, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{B2buaScene, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\na=sendonly\r\n";

/// Established call → alice UPDATEs → the B2BUA relays → bob answers **481**.
/// The 481 must be relayed back to alice (pre-fix: silently dropped, alice
/// times out) and the call must survive — the failure of one in-dialog
/// transaction is reported, nothing more. A follow-up INFO answered **488**
/// proves the same symmetry for another method, and a normal BYE teardown
/// proves the dialogs stayed usable.
#[tokio::test]
async fn relayed_update_481_reaches_requester_and_call_survives() {
    let s = B2buaScene::new("b2bua-update-481-relay").await;
    let mut dialog = s.establish().await;
    assert_eq!(s.b2bua.active_calls(), 1, "call established");

    // ── alice UPDATEs; bob rejects 481 ──
    let mut update = dialog.request(InDialogMethod::Update, Some(REOFFER)).await;
    s.bob.receive("UPDATE").await.respond(481, "Call/Transaction Does Not Exist").await;

    // The non-2xx final is relayed to the requester (GAP-P8b-5).
    update.expect(481).await;

    // The call is NOT torn down by the failed UPDATE transaction.
    assert_eq!(s.b2bua.active_calls(), 1, "481 to a relayed UPDATE kept the call up");

    // ── same symmetry for INFO: bob rejects 488 ──
    let mut info = dialog.request(InDialogMethod::Info, None).await;
    s.bob.receive("INFO").await.respond(488, "Not Acceptable Here").await;
    info.expect(488).await;
    assert_eq!(s.b2bua.active_calls(), 1, "488 to a relayed INFO kept the call up");

    // ── the dialogs are still live: a normal BYE teardown completes ──
    s.hangup(&mut dialog).await;
    let _report = s.finish().await;
}

/// The downstream `bc_rc_update_then_bye` direction: **bob** relays an UPDATE
/// and **alice** answers the non-2xx (481). The final must reach bob — the
/// pending-relay snapshot lives on the a-leg dialog and correlates the a-side
/// response back to the b-side requester exactly like the reverse direction.
#[tokio::test]
async fn relayed_update_481_from_answering_side_reaches_bob() {
    let h = Harness::new("b2bua-update-481-from-a");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5070).start(&h, "b2bua", "127.0.0.1:5080").await;

    // ── call setup (hand-rolled: we need bob's UAS-side dialog) ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();
    assert_eq!(b2bua.active_calls(), 1, "call established");

    // ── bob UPDATEs; alice rejects 481; the 481 reaches bob ──
    let mut update = bob_dialog.request(InDialogMethod::Update, Some(REOFFER)).await;
    alice.receive("UPDATE").await.respond(481, "Call/Transaction Does Not Exist").await;
    update.expect(481).await;
    assert_eq!(b2bua.active_calls(), 1, "b→a UPDATE 481 kept the call up");

    // ── teardown still works ──
    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
}

/// GAP-P8b-2: an UPDATE arriving on the surviving early dialog while a
/// failover reroute is pending is answered **491 Request Pending** locally
/// (`update-peer-unavailable`) — the failed b-leg is terminated and the
/// replacement leg has no usable dialog yet, so relaying would go into the
/// void. The reroute then completes normally and the call establishes.
#[tokio::test]
async fn update_during_pending_reroute_gets_491_and_reroute_completes() {
    let h = Harness::new("b2bua-update-failover-491");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let carol = h.agent("carol", "127.0.0.1:5070").await; // first target — fails
    let bob = h.agent("bob", "127.0.0.1:5071").await; // reroute target

    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                // The callback context is what makes the 486 consult
                // /call/failure instead of relaying immediately.
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("ctx-update-491".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_| CallTreatment::Route(route_to("127.0.0.1", 5071)))
            .build(),
    );
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5080").await;

    // ── alice INVITEs; carol rings (early dialog toward alice), then fails ──
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut carol_uas = carol.receive("INVITE").await;
    carol_uas.respond(180, "Ringing").await;
    let ringing = call.expect(180).await;
    let early_atag = ringing.to.tag.clone().expect("a-facing early-dialog tag");

    carol_uas.respond(486, "Busy Here").await;
    carol.receive("ACK").await; // b2bua ACKs the failed b-leg final

    // The reroute INVITE reaches bob — the replacement leg exists but is still
    // Trying (no usable dialog): the failover is PENDING.
    let mut bob_uas = bob.receive("INVITE").await;

    // ── alice UPDATEs the surviving early dialog mid-reroute → local 491 ──
    let mut update = call
        .send_request(InDialogMethod::Update)
        .with_to_tag(&early_atag)
        .with_sdp(REOFFER)
        .send()
        .await;
    update.expect(491).await;

    // Neither target saw the UPDATE — it was answered locally.
    // (bob would receive it on his pristine INVITE txn; carol is dead.)

    // ── the reroute completes normally ──
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    assert_eq!(b2bua.active_calls(), 1, "reroute established after the 491'd UPDATE");

    // ── teardown ──
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let _report = h.finish().await;
}
