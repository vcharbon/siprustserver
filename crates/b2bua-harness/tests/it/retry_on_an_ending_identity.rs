//! A second initial INVITE on one Call-ID and From tag. A merged copy of an
//! INVITE matches its From tag, Call-ID and CSeq (RFC 3261 §8.2.2.2); a
//! caller's retry after a challenge (§22.2) carries the next CSeq
//! (§8.1.3.5) and is a new request: the admission ladder judges it and,
//! admitted, it is routed as a call of its own, even while the challenged
//! call's last turn is still finishing. Each attempt writes its own CDR. A
//! copy reaching a call still here is answered 482, and a new request on the
//! identity of a call still live is answered 500 with a Retry-After.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::admission::Class;
use b2bua::cdr::{CdrRecord, CdrWriter};
use b2bua::metrics::RemovalClass;
use b2bua::new_calls::Refusal;
use b2bua::store::{StoreFaultPoint, StoreFaults};
use b2bua_harness::{settle_until, stated_by_response, B2buaScene, B2buaSut};
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::{ClientInvite, WaiverScope};
use tokio::sync::watch;

const CALL_ID: &str = "same-identity@127.0.0.1";
const FROM_TAG: &str = "same-identity-from-tag";
/// Past the simulated transit of a request and its answer.
const TRANSIT: Duration = Duration::from_millis(500);

/// Holds every CDR write until opened: the turn that ends a call parks
/// there, after its final reached the caller and before its release.
struct GatedCdr {
    open: watch::Receiver<bool>,
}

#[async_trait]
impl CdrWriter for GatedCdr {
    async fn write(&self, _call: &call::Call, _terminated_at: i64) {
        let mut open = self.open.clone();
        let _ = open.wait_for(|o| *o).await;
    }
    async fn read_all(&self) -> Vec<CdrRecord> {
        Vec::new()
    }
}

/// The scene of a callee challenging the first INVITE, the CDR writes gated
/// by the returned sender.
async fn challenged_scene(name: &str) -> (B2buaScene, watch::Sender<bool>) {
    let (open, gate) = watch::channel(false);
    let s = B2buaScene::with_b2bua(name, |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).cdr_tap(Arc::new(GatedCdr { open: gate }))
    })
    .await;
    (s, open)
}

/// alice's first INVITE is challenged by bob; its call's last turn parks on
/// the gated CDR write.
async fn challenge_the_first_invite(s: &B2buaScene) {
    let mut first = s
        .alice
        .invite(&s.bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER_SDP)
        .through(s.b2bua.addr)
        .send()
        .await;
    s.bob
        .receive("INVITE")
        .await
        .respond(407, "Proxy Authentication Required")
        .with_header("Proxy-Authenticate", "Digest realm=\"bob\", nonce=\"n1\"")
        .await;
    s.bob.receive("ACK").await;
    first.expect(407).await;
}

/// alice's authenticated retry under the challenged INVITE's identity.
async fn send_the_retry(s: &B2buaScene) -> ClientInvite {
    s.alice
        .invite(&s.bob)
        .identity(CALL_ID, FROM_TAG)
        .cseq(2)
        .with_header(
            "Proxy-Authorization",
            "Digest username=\"alice\", realm=\"bob\", nonce=\"n1\", uri=\"sip:bob@127.0.0.1\", \
             response=\"0123456789abcdef0123456789abcdef\"",
        )
        .with_sdp(OFFER_SDP)
        .through(s.b2bua.addr)
        .send()
        .await
}

/// The callee challenges the first INVITE; the caller's authenticated retry
/// reaches the node while the challenged call's last turn is still writing
/// its CDR. The retry is judged, routed to the callee and answered: two
/// accepted calls, no copy refused, one CDR each.
#[tokio::test(start_paused = true)]
async fn a_retry_inside_the_challenged_calls_last_turn_is_routed_as_a_new_call() {
    let (s, open) = challenged_scene("retry-inside-challenged-turn").await;
    challenge_the_first_invite(&s).await;
    let mut retry = send_the_retry(&s).await;
    s.h.advance(TRANSIT).await;
    assert_eq!(s.b2bua.active_calls(), 1, "the challenged call's last turn is still running");
    open.send_replace(true);

    let mut uas = s.bob.try_receive("INVITE").await.expect("the retry is routed to the callee");
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    retry.expect(200).await;
    let mut dialog = retry.ack().await;
    s.bob.receive("ACK").await;
    s.hangup(&mut dialog).await;

    settle_until(|| s.b2bua.cdr_records().len() == 2 && s.b2bua.is_reaped()).await;
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.accepted(Class::Normal), 2, "each attempt is a call of its own");
    assert_eq!(counts.refused_copies(), 0, "the retry is no copy");
    assert_eq!(counts.total(), 2, "one count per INVITE sent");
    let cdrs = s.b2bua.cdr_records();
    assert_eq!(cdrs.len(), 2, "one CDR per attempt: {cdrs:?}");
    let _ = s.finish().await;
}

/// alice's second tagless INVITE on her call's Call-ID and From tag is the
/// corner under test: the audit reads it as a request of her dialog with an
/// out-of-order CSeq and, once the dialog is confirmed, no To-tag.
fn waive_second_invite_on_the_identity(s: &B2buaScene, confirmed: bool, why: &str) {
    let rules: &[&str] = if confirmed {
        &["in-dialog-to-tag", "cseq-in-dialog-order"]
    } else {
        &["cseq-in-dialog-order"]
    };
    for rule in rules {
        s.h.waive(WaiverScope::rule(*rule, why).on_party("alice"));
    }
}

/// Why a merged copy is sent: it stands for one a forking hop merged back.
const MERGED_COPY: &str =
    "alice's INVITE resent on a new branch with its CSeq stands for a copy a \
                           forking hop merged back (RFC 3261 §8.2.2.2), the request under test";

/// A new INVITE on an established call's Call-ID and From tag, with a new
/// CSeq, is a new request the call's identity is still in use for: it is
/// judged, answered 500 with a Retry-After (RFC 3261 §14.2) and counted as
/// a rejected new call; the call lives on.
#[tokio::test(start_paused = true)]
async fn a_new_request_on_a_live_calls_identity_is_answered_retry_later() {
    let s = B2buaScene::new("new-request-on-live-identity").await;
    waive_second_invite_on_the_identity(
        &s,
        true,
        "alice reuses her live call's Call-ID and From tag for a new INVITE, the request under \
         test",
    );
    let mut call = s
        .alice
        .invite(&s.bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER_SDP)
        .through(s.b2bua.addr)
        .send()
        .await;
    s.bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;

    let mut second = s
        .alice
        .invite(&s.bob)
        .identity(CALL_ID, FROM_TAG)
        .cseq(20)
        .with_sdp(OFFER_SDP)
        .through(s.b2bua.addr)
        .send()
        .await;
    let refusal = second.try_expect(500).await.expect("the new request is answered 500");
    assert!(
        stated_by_response(&refusal, "Retry-After").is_some(),
        "the 500 states when to retry: {refusal:?}"
    );
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.rejected(Refusal::IdentityInUse, Class::Normal), 1);
    assert_eq!(counts.accepted(Class::Normal), 1, "only the live call was accepted");
    assert_eq!(counts.refused_copies(), 0, "a new CSeq is no copy");
    assert_eq!(s.b2bua.active_calls(), 1, "the call lives on");

    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1 && s.b2bua.is_reaped()).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "one CDR, the call's");
    let _ = s.finish().await;
}

/// The caller CANCELs its retry while it waits for the challenged call's
/// release (RFC 3261 §9.1): the challenged call's removal leaves the retry's
/// cancellation standing, so the retry is born cancelled, asks no decision
/// and never reaches the callee. Each attempt writes its CDR.
#[tokio::test(start_paused = true)]
async fn a_retry_canceled_while_waiting_for_the_release_is_not_routed() {
    let (s, open) = challenged_scene("retry-canceled-behind-release").await;
    challenge_the_first_invite(&s).await;
    let mut retry = send_the_retry(&s).await;
    s.h.advance(TRANSIT).await;
    let mut cxl = retry.cancel().await;
    cxl.expect(200).await;
    retry.expect(487).await;
    assert_eq!(s.b2bua.active_calls(), 1, "the challenged call's last turn is still running");
    open.send_replace(true);

    settle_until(|| s.b2bua.cdr_records().len() == 2 && s.b2bua.is_reaped()).await;
    assert!(
        s.bob.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "the cancelled retry never reaches the callee"
    );
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.accepted(Class::Normal), 1, "only the challenged INVITE was accepted");
    assert_eq!(counts.cancelled(Class::Normal), 1, "the retry counts as cancelled");
    assert_eq!(s.b2bua.cdr_records().len(), 2, "one CDR per attempt");
    let _ = s.finish().await;
}

/// The caller's INVITE reaches the node again on another branch with its
/// CSeq while the callee rings (a request a forking hop merged back, RFC 3261
/// §8.2.2.2): the copy is answered 482 and counted as a refused copy; the
/// original goes on to its 200.
#[tokio::test(start_paused = true)]
async fn a_merged_copy_of_a_ringing_calls_invite_is_answered_482() {
    let s = B2buaScene::new("merged-copy-of-ringing-invite").await;
    waive_second_invite_on_the_identity(&s, false, MERGED_COPY);
    let mut call = s
        .alice
        .invite(&s.bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER_SDP)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut copy = s
        .alice
        .invite(&s.bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER_SDP)
        .through(s.b2bua.addr)
        .send()
        .await;
    copy.try_expect(482).await.expect("the merged copy is answered 482");
    let counts = s.b2bua.new_calls();
    assert_eq!(counts.refused_copies(), 1, "the copy is counted as a copy");
    assert_eq!(counts.total(), 1, "the copy is no new call");

    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1 && s.b2bua.is_reaped()).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "one CDR, the call's");
    let _ = s.finish().await;
}

/// The call store faults on the turn of a copy reaching a ringing call
/// (RFC 3261 §8.2.2.2): the copy is answered the fault's 500, and the call
/// keeps its queue — no orphan release tears it down — and goes on to its
/// 200.
#[tokio::test(start_paused = true)]
async fn a_store_fault_on_a_copys_turn_leaves_the_call_its_queue() {
    let faults = StoreFaults::default();
    let s = B2buaScene::with_b2bua("store-fault-on-copy-turn", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).with_store_faults(faults.clone())
    })
    .await;
    waive_second_invite_on_the_identity(&s, false, MERGED_COPY);
    let mut call = s
        .alice
        .invite(&s.bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER_SDP)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    faults.arm(StoreFaultPoint::LiveInitialInvite);
    let mut copy = s
        .alice
        .invite(&s.bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER_SDP)
        .through(s.b2bua.addr)
        .send()
        .await;
    copy.try_expect(500).await.expect("the copy is answered the store fault's 500");
    faults.disarm_all();
    assert_eq!(
        s.b2bua.metrics().removals_of_total(RemovalClass::Orphan),
        0,
        "the live call's queue is not released as an orphan's"
    );

    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1 && s.b2bua.is_reaped()).await;
    let _ = s.finish().await;
}
