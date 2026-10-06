//! An initial INVITE re-offered on the Call-ID and From tag of a call that
//! just ended: no To tag, a new branch, and either the same CSeq (a caller
//! retrying after its own CANCEL) or the next one (§8.1.3.4 after a 3xx, or
//! after a 4xx). The re-offer derives the first call's callRef.
//!
//! Whether the first call is still resident decides the outcome; the state of
//! its INVITE server transaction does not:
//! - same CSeq while the first call is resident (waiting for its callee's
//!   487), inside Timer I: answered 482 as a merged copy
//!   (`FIXME(copy-after-end)` in `router/admit.rs`);
//! - same CSeq after the release, the server transaction still in Timer I:
//!   a new call;
//! - same CSeq after the release and past Timer I: a new call;
//! - CSeq+1 right after the ACK of a 3xx or a 404: a new call, routed after
//!   the first call's release.
//!
//! A new call is routed, ended and recorded on its own, its outgoing leg under
//! an identity of its own.

use std::time::Duration;

use b2bua::admission::Class;
use b2bua::metrics::RemovalClass;
use b2bua_harness::{settle_until, B2buaScene};
use call::CdrEventType;
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::{ClientInvite, ServerTxn, WaiverScope};

const CALL_ID: &str = "reoffer@127.0.0.1";
const FROM_TAG: &str = "reoffer-from-tag";
/// The UDP T4, the INVITE server transaction's Timer I (RFC 3261 §17.2.1).
const T4: Duration = Duration::from_secs(5);

/// The re-offer is the caller behaviour under test: its second tagless INVITE
/// on one Call-ID and From tag is read by the audit as a request of the first
/// one's dialog.
fn waive_reoffer(s: &B2buaScene) {
    s.h.waive(
        WaiverScope::rule(
            "cseq-in-dialog-order",
            "alice re-offers her INVITE on the ended call's Call-ID and From tag, the \
             request under test",
        )
        .on_party("alice"),
    );
}

/// The caller's INVITE under the fixed identity with `cseq`, on a fresh branch.
async fn offer(s: &B2buaScene, cseq: u32) -> ClientInvite {
    s.alice
        .invite(&s.bob)
        .identity(CALL_ID, FROM_TAG)
        .cseq(cseq)
        .with_sdp(OFFER_SDP)
        .through(s.b2bua.addr)
        .send()
        .await
}

/// The outgoing leg's identity as the callee sees it: Call-ID and From tag.
fn leg_identity(uas: &ServerTxn) -> (String, String) {
    let req = uas.request();
    (req.call_id().to_string(), req.from().tag().unwrap_or_default().to_string())
}

/// The first call rings and the caller CANCELs it: the caller holds its 487
/// (ACKed), the callee has answered the CANCEL and still owes its 487.
async fn ring_then_cancel(s: &B2buaScene) -> ServerTxn {
    let mut first = offer(s, 1).await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    first.expect(180).await;
    let mut cxl = first.cancel().await;
    cxl.expect(200).await;
    first.expect(487).await;
    s.bob.receive("CANCEL").await.respond(200, "OK").await;
    uas
}

/// The callee ends its cancelled INVITE: 487 and the hop ACK.
async fn callee_terminates(mut uas: ServerTxn) {
    uas.respond(487, "Request Terminated").await;
    uas.expect_ack().await;
}

/// The re-offer reaches the callee under an identity of its own; it is
/// answered, ACKed and hung up by the caller.
async fn reoffer_is_a_new_call(
    s: &B2buaScene,
    mut reoffer: ClientInvite,
    first_leg: &(String, String),
) {
    let mut uas = s.bob.try_receive("INVITE").await.expect("the re-offer is routed to the callee");
    assert_eq!(
        s.b2bua.metrics().removals_of_total(RemovalClass::Terminated),
        1,
        "routed after the first call's release"
    );
    assert_ne!(&leg_identity(&uas), first_leg, "the new call's outgoing leg has a fresh identity");
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    reoffer.expect(200).await;
    let mut dialog = reoffer.ack().await;
    s.bob.receive("ACK").await;
    s.hangup(&mut dialog).await;
}

/// Two calls under one callRef, the second answered, each with its own CDR.
fn assert_two_calls(s: &B2buaScene) {
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.accepted(Class::Normal), 2, "each INVITE is a call of its own");
    assert_eq!(counts.refused_copies(), 0, "the re-offer is no copy");
    let cdrs = s.b2bua.cdr_records();
    assert_eq!(cdrs.len(), 2, "one CDR per call: {cdrs:?}");
    let answered = |i: usize| cdrs[i].events.iter().any(|e| e.event_type == CdrEventType::Answer);
    assert!(!answered(0) && answered(1), "the first call is cancelled, the second answered");
}

/// INVITE, CANCEL, 487, ACK; the call is released and its INVITE server
/// transaction ends (Timer I); the caller re-offers on the same Call-ID, From
/// tag and CSeq on a new branch: a new call.
#[tokio::test(start_paused = true)]
async fn a_reoffer_after_the_release_of_a_cancelled_call_is_a_new_call() {
    let s = B2buaScene::new("reoffer-after-release").await;
    waive_reoffer(&s);
    let uas = ring_then_cancel(&s).await;
    let acked_at = tokio::time::Instant::now();
    let first_leg = leg_identity(&uas);
    callee_terminates(uas).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1 && s.b2bua.is_reaped()).await;
    assert_eq!(s.b2bua.active_calls(), 0, "the first call is released");
    s.h.advance(2 * T4 - acked_at.elapsed()).await;
    assert!(acked_at.elapsed() > T4, "past Timer I: {:?}", acked_at.elapsed());

    let reoffer = offer(&s, 1).await;
    reoffer_is_a_new_call(&s, reoffer, &first_leg).await;
    settle_until(|| s.b2bua.cdr_records().len() == 2 && s.b2bua.is_reaped()).await;
    assert_two_calls(&s);
    let _ = s.finish().await;
}

/// The re-offer arrives after the caller's ACK, inside Timer I, while the
/// first call still waits for its callee's 487: it is answered 482 as a
/// merged copy of the first INVITE (`FIXME(copy-after-end)`), and the first
/// call ends on its own.
#[tokio::test(start_paused = true)]
async fn a_reoffer_before_the_release_inside_timer_i_is_answered_482() {
    let s = B2buaScene::new("reoffer-inside-timer-i-before-release").await;
    waive_reoffer(&s);
    let uas = ring_then_cancel(&s).await;
    assert_eq!(s.b2bua.active_calls(), 1, "the first call waits for its callee's 487");

    let mut reoffer = offer(&s, 1).await;
    reoffer.try_expect(482).await.expect("the re-offer is answered 482");
    callee_terminates(uas).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1 && s.b2bua.is_reaped()).await;
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.refused_copies(), 1, "the re-offer is counted as a copy");
    assert_eq!(counts.accepted(Class::Normal), 1, "only the first INVITE is a call");
    let _ = s.finish().await;
}

/// The re-offer arrives after the first call's release but inside Timer I of
/// its INVITE server transaction: a new call.
#[tokio::test(start_paused = true)]
async fn a_reoffer_after_the_release_inside_timer_i_is_a_new_call() {
    let s = B2buaScene::new("reoffer-inside-timer-i-after-release").await;
    waive_reoffer(&s);
    let uas = ring_then_cancel(&s).await;
    let acked_at = tokio::time::Instant::now();
    let first_leg = leg_identity(&uas);
    callee_terminates(uas).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1 && s.b2bua.is_reaped()).await;
    assert!(acked_at.elapsed() < T4, "still inside Timer I: {:?}", acked_at.elapsed());

    let reoffer = offer(&s, 1).await;
    reoffer_is_a_new_call(&s, reoffer, &first_leg).await;
    settle_until(|| s.b2bua.cdr_records().len() == 2 && s.b2bua.is_reaped()).await;
    assert_two_calls(&s);
    let _ = s.finish().await;
}

/// The callee answers the first INVITE with `status`, relayed to the caller
/// and ACKed hop by hop; the caller retries with CSeq+1 right away.
async fn rejected_then_retried(
    s: &B2buaScene,
    status: u16,
    reason: &str,
    extra: Option<(&str, &str)>,
) {
    let mut first = offer(s, 1).await;
    let mut uas = s.bob.receive("INVITE").await;
    let first_leg = leg_identity(&uas);
    let mut final_resp = uas.respond(status, reason);
    if let Some((name, value)) = extra {
        final_resp = final_resp.with_header(name, value);
    }
    final_resp.await;
    uas.expect_ack().await;
    first.expect(status).await;

    let retry = offer(s, 2).await;
    reoffer_is_a_new_call(s, retry, &first_leg).await;
    settle_until(|| s.b2bua.cdr_records().len() == 2 && s.b2bua.is_reaped()).await;
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.accepted(Class::Normal), 2, "each INVITE is a call of its own");
    assert_eq!(counts.refused_copies(), 0, "the retry is no copy");
    assert_eq!(s.b2bua.cdr_records().len(), 2, "one CDR per call");
}

/// §8.1.3.4: after a 302 the caller retries on the same Call-ID and From tag
/// with CSeq+1, right after its ACK: a new call.
#[tokio::test(start_paused = true)]
async fn a_cseq_plus_one_reoffer_after_a_3xx_is_a_new_call() {
    let s = B2buaScene::new("reoffer-cseq-plus-one-after-3xx").await;
    rejected_then_retried(
        &s,
        302,
        "Moved Temporarily",
        Some(("Contact", "<sip:bob@127.0.0.1:5070>")),
    )
    .await;
    let _ = s.finish().await;
}

/// After a 404 the caller retries on the same Call-ID and From tag with
/// CSeq+1, right after its ACK: a new call.
#[tokio::test(start_paused = true)]
async fn a_cseq_plus_one_reoffer_after_a_404_is_a_new_call() {
    let s = B2buaScene::new("reoffer-cseq-plus-one-after-404").await;
    rejected_then_retried(&s, 404, "Not Found", None).await;
    let _ = s.finish().await;
}
