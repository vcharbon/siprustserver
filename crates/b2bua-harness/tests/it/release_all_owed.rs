//! What an operator's release-all still sends on an answered call (ADR-0050
//! §10): no BYE, but the ACK a callee's 2xx is owed (RFC 3261 §13.2.2.4), on
//! the call's answer and on a relayed re-INVITE's, and the CANCEL of a leg
//! still ringing (RFC 3261 §9.1), as the abort of a call in setup sends one.
//!
//! The answered call's parties are left holding a dialog the SUT dropped:
//! that is what the test asserts. Each ends its side with a BYE, and the
//! `481` that answers it terminates the dialog (RFC 3261 §12.2.1.2).

use b2bua::release_all::ReleasedAll;
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use call::TerminationCause;
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use sip_message::generators::InDialogMethod;

use crate::release_all::{answered, sent_since};

const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";

const CHARLIE_PORT: u16 = 5667;

/// The SUT sent no BYE from the `mark`-th recorded datagram on.
fn assert_no_bye_since(s: &B2buaScene, mark: usize) {
    let sent = sent_since(s, mark);
    assert!(!sent.iter().any(|l| l.starts_with("BYE")), "no BYE: {sent:?}");
}

/// The cause of the one record the SUT wrote.
fn the_cause(s: &B2buaScene) -> Option<TerminationCause> {
    let records = s.b2bua.cdr_records();
    let [record] = records.as_slice() else { panic!("one record: {records:#?}") };
    record.termination.as_ref().map(|t| t.cause)
}

/// The release lands after the callee's 200 is relayed and before the
/// caller's ACK: the SUT ACKs the callee itself, so the callee stops
/// repeating its 200 and sends no BYE.
#[tokio::test(start_paused = true)]
async fn a_release_before_the_callers_ack_acks_the_callees_answer() {
    let s = B2buaScene::new("release-all-answer-ack-owed").await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.expect(200).await;

    let mark = s.h.wire_entries().len();
    assert_eq!(s.b2bua.release_all(), ReleasedAll { released: 1, aborted: 0 });
    s.bob.receive("ACK").await;
    let mut alice = call.ack().await;
    let mut bob = uas.dialog();
    settle_until(|| s.b2bua.is_reaped()).await;
    assert_no_bye_since(&s, mark);
    assert_eq!(the_cause(&s), Some(TerminationCause::OperatorRelease));

    alice.bye().await.expect(481).await;
    bob.bye().await.expect(481).await;
    let _report = s.finish().await;
}

/// The release lands after the callee's 200 to a relayed re-INVITE is relayed
/// and before the caller's ACK: the SUT ACKs that 200 itself.
#[tokio::test(start_paused = true)]
async fn a_release_before_the_callers_reinvite_ack_acks_the_callees_answer() {
    let s = B2buaScene::new("release-all-reinvite-ack-owed").await;
    let (mut alice, mut bob) = answered(&s).await;
    let mut round = alice.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let mut at_bob = s.bob.receive("INVITE").await;
    at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    round.expect(200).await;

    let mark = s.h.wire_entries().len();
    assert_eq!(s.b2bua.release_all(), ReleasedAll { released: 1, aborted: 0 });
    s.bob.receive("ACK").await;
    alice.ack(None).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    assert_no_bye_since(&s, mark);
    assert_eq!(the_cause(&s), Some(TerminationCause::OperatorRelease));

    alice.bye().await.expect(481).await;
    bob.bye().await.expect(481).await;
    let _report = s.finish().await;
}

/// An answered call whose transfer target still rings: the release CANCELs
/// the ringing leg, whose `487` is ACKed, and BYEs neither party.
#[tokio::test(start_paused = true)]
async fn a_release_cancels_a_leg_still_ringing_on_an_answered_call() {
    let s = B2buaScene::with_b2bua("release-all-ringing-target", |bob_port| {
        B2buaSut::route_all_with_refer("127.0.0.1", bob_port)
    })
    .await;
    let charlie =
        s.h.agent("charlie", &format!("127.0.0.1:{CHARLIE_PORT}")).await.transfer_target();
    let (mut alice, mut bob) = answered(&s).await;

    let x_api = format!(
        r#"{{"refer_key":"refer-allow-c","destination":{{"host":"127.0.0.1","port":{CHARLIE_PORT}}}}}"#
    );
    let mut refer = bob
        .send_request(InDialogMethod::Refer)
        .with_header("Refer-To", &format!("<sip:charlie@127.0.0.1:{CHARLIE_PORT}>"))
        .with_header("X-Api-Call", &x_api)
        .send()
        .await;
    refer.expect(202).await;
    s.bob.receive("NOTIFY").await.respond(200, "OK").await;
    let mut at_charlie = charlie.receive("INVITE").await;
    at_charlie.respond(180, "Ringing").await;

    let mark = s.h.wire_entries().len();
    assert_eq!(s.b2bua.release_all(), ReleasedAll { released: 1, aborted: 0 });
    charlie.receive("CANCEL").await.respond(200, "OK").await;
    at_charlie.respond(487, "Request Terminated").await;
    charlie.receive("ACK").await;
    settle_until(|| s.b2bua.is_reaped()).await;
    assert_no_bye_since(&s, mark);
    assert_eq!(the_cause(&s), Some(TerminationCause::OperatorRelease));

    alice.bye().await.expect(481).await;
    bob.bye().await.expect(481).await;
    let _report = s.finish().await;
}
