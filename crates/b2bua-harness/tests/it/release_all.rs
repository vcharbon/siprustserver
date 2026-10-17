//! An operator's release-all (ADR-0050 §10). An answered call ends with
//! nothing sent: one record closed under `operator_release`, its limiter hold
//! released, no BYE to either party, and a later request in its dialog draws
//! `481` (RFC 3261 §12.2.2). A call in setup is aborted as usual: `503` to the
//! caller, CANCEL to the ringing callee, whose `487` is ACKed.
//!
//! The answered call's parties are left holding a dialog the SUT dropped:
//! that is what the test asserts. Each ends its side with a BYE, and the
//! `481` that answers it terminates the dialog (RFC 3261 §12.2.1.2).

use std::time::Duration;

use b2bua::release_all::{ReleasedAll, REASON};
use b2bua_harness::{settle_until, B2buaScene, DEFAULT_LIMITER_ID};
use call::{CdrEventType, TerminationCause};
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::Dialog;

/// An answered call: the caller's dialog and the callee's.
pub(crate) async fn answered(s: &B2buaScene) -> (Dialog, Dialog) {
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    s.bob.receive("ACK").await;
    (dialog, uas.dialog())
}

/// What the SUT put on the wire from the `mark`-th recorded datagram on.
pub(crate) fn sent_since(s: &B2buaScene, mark: usize) -> Vec<String> {
    s.h.wire_entries()
        .into_iter()
        .skip(mark)
        .filter(|e| e.from == s.b2bua.addr)
        .map(|e| String::from_utf8_lossy(&e.raw).lines().next().unwrap_or_default().to_string())
        .collect()
}

/// `b2bua_operator_released_total` as the SUT renders it: `(released, aborted)`.
fn operator_released(s: &B2buaScene) -> (u64, u64) {
    let text = s.b2bua.metrics().prometheus_text();
    let series = |kind: &str| {
        let head = format!("b2bua_operator_released_total{{kind=\"{kind}\"}} ");
        let line = text.lines().find_map(|l| l.strip_prefix(head.as_str()).map(str::to_owned));
        line.unwrap_or_else(|| panic!("{head} rendered")).parse().unwrap()
    };
    (series("released"), series("aborted"))
}

/// The held count of the limiter every routed call holds.
pub(crate) fn held(s: &B2buaScene) -> i64 {
    s.b2bua.limiter_store().expect("the default limiter's store").held(DEFAULT_LIMITER_ID)
}

#[tokio::test(start_paused = true)]
async fn an_answered_call_released_by_the_operator_ends_with_its_record_and_nothing_sent() {
    let s = B2buaScene::new("release-all-answered").await;
    let (mut alice, mut bob) = answered(&s).await;
    assert_eq!(held(&s), 1, "the answered call holds its limiter");

    let mark = s.h.wire_entries().len();
    assert_eq!(operator_released(&s), (0, 0));
    assert_eq!(s.b2bua.release_all(), ReleasedAll { released: 1, aborted: 0 });
    assert_eq!(operator_released(&s), (1, 0));
    settle_until(|| s.b2bua.is_reaped()).await;
    s.h.advance(Duration::from_secs(1)).await;
    assert_eq!(sent_since(&s, mark), Vec::<String>::new(), "nothing sent to either party");
    assert_eq!(held(&s), 0, "the limiter hold is released");

    let records = s.b2bua.cdr_records();
    let [record] = records.as_slice() else { panic!("one record: {records:#?}") };
    let termination = record.termination.as_ref().expect("a closed record");
    assert_eq!(termination.cause, TerminationCause::OperatorRelease);
    assert_eq!(termination.by_leg, None);
    let last = record.events.last().map(|e| (e.event_type, e.reason.as_deref()));
    assert_eq!(last, Some((CdrEventType::Bye, Some(REASON))));

    // ── each party ends the dialog the SUT dropped ──────────────────────────
    alice.bye().await.expect(481).await;
    bob.bye().await.expect(481).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    assert_eq!(s.b2bua.cdr_records().len(), 1, "a request for the released call writes nothing");
    let _report = s.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_call_in_setup_is_aborted_with_its_final_and_its_callee_cancelled() {
    let s = B2buaScene::new("release-all-setup").await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    assert_eq!(s.b2bua.release_all(), ReleasedAll { released: 0, aborted: 1 });
    assert_eq!(operator_released(&s), (0, 1));
    let mut cancel = s.bob.receive("CANCEL").await;
    cancel.respond(200, "OK").await;
    let refused = call.expect(503).await;
    assert_eq!(refused.reason(), "Service Unavailable");
    uas.respond(487, "Request Terminated").await;
    s.bob.receive("ACK").await;

    settle_until(|| s.b2bua.is_reaped()).await;
    assert_eq!(held(&s), 0, "the limiter hold is released");
    let records = s.b2bua.cdr_records();
    let [record] = records.as_slice() else { panic!("one record: {records:#?}") };
    let cause = record.termination.as_ref().map(|t| t.cause);
    assert_eq!(cause, Some(TerminationCause::OperatorRelease));
    let _report = s.finish().await;
}

/// Two requests before either reaches the call: the first aborts it, the
/// second finds it ending and does nothing. One CANCEL, one `503`, one record.
#[tokio::test(start_paused = true)]
async fn a_second_release_reaching_an_ending_call_does_nothing() {
    let s = B2buaScene::new("release-all-twice").await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let mark = s.h.wire_entries().len();
    let reached = ReleasedAll { released: 0, aborted: 1 };
    assert_eq!((s.b2bua.release_all(), s.b2bua.release_all()), (reached, reached));
    let mut cancel = s.bob.receive("CANCEL").await;
    cancel.respond(200, "OK").await;
    call.expect(503).await;
    uas.respond(487, "Request Terminated").await;
    s.bob.receive("ACK").await;
    settle_until(|| s.b2bua.is_reaped()).await;
    s.h.advance(Duration::from_secs(1)).await;

    let sent = sent_since(&s, mark);
    let count = |start: &str| sent.iter().filter(|l| l.starts_with(start)).count();
    assert_eq!((count("CANCEL"), count("SIP/2.0 503")), (1, 1), "{sent:?}");
    assert_eq!(s.b2bua.cdr_records().len(), 1);
    let _report = s.finish().await;
}

/// A draining worker releases its calls as a serving one does.
#[tokio::test(start_paused = true)]
async fn a_draining_worker_releases_its_calls() {
    let s = B2buaScene::new("release-all-draining").await;
    let (mut alice, mut bob) = answered(&s).await;
    s.b2bua.readiness().set_draining();

    let mark = s.h.wire_entries().len();
    assert_eq!(s.b2bua.release_all(), ReleasedAll { released: 1, aborted: 0 });
    settle_until(|| s.b2bua.is_reaped()).await;
    assert_eq!(sent_since(&s, mark), Vec::<String>::new(), "nothing sent to either party");
    let records = s.b2bua.cdr_records();
    let cause = records.iter().map(|r| r.termination.as_ref().map(|t| t.cause)).collect::<Vec<_>>();
    assert_eq!(cause, [Some(TerminationCause::OperatorRelease)]);
    alice.bye().await.expect(481).await;
    bob.bye().await.expect(481).await;
    let _report = s.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_release_all_with_no_call_reaches_none() {
    let s = B2buaScene::new("release-all-idle").await;
    let mut dialog = s.establish().await;
    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    assert_eq!(s.b2bua.release_all(), ReleasedAll::default());
    let _report = s.finish().await;
}
