//! An INVITE whose only session description is disposed `early-session`
//! (RFC 3959) makes no offer: the offer/answer model binds only bodies whose
//! disposition is `session` (RFC 3261 §13.2.1, §20.11). The B2BUA relays the
//! body to the callee as it came, beside its disposition, and the call runs as
//! a delayed offer: the callee's 200 offers and the caller's ACK answers
//! (RFC 3264 §4), each relayed. A bare `early-session` body in a provisional,
//! an UPDATE or a re-INVITE is no session description either: it rides as
//! written and moves no session state the stack restates later (RFC 3264 §8).

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Dialog, Harness};
use sip_message::generators::InDialogMethod;
use sip_message::HeaderName;

const EARLY: &str = "v=0\r\no=alice 301 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
                     t=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const OFFER: &str = "v=0\r\no=bob 401 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
                     t=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=alice 302 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
                      t=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";

fn line(txn: &scenario_harness::ServerTxn, name: &str) -> Option<String> {
    txn.request().raw(HeaderName::from(name)).next().map(str::to_string)
}

#[tokio::test(start_paused = true)]
async fn an_invite_whose_only_description_is_early_session_runs_as_a_delayed_offer() {
    let h = Harness::new("b2bua-early-session-body");
    let alice = h.agent("alice", "127.0.0.1:5471").await;
    let bob = h.agent("bob", "127.0.0.1:5481").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5481).start(&h, "b2bua", "127.0.0.1:5491").await;

    let mut call = alice
        .invite(&bob)
        .with_body("application/sdp", EARLY.as_bytes().to_vec())
        .with_header("Content-Disposition", "early-session")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    assert_eq!(uas.request().body().as_ref(), EARLY.as_bytes(), "the body rides as it came");
    assert_eq!(line(&uas, "Content-Disposition").as_deref(), Some("early-session"));
    assert_eq!(uas.request().sdp(), None, "the callee's INVITE makes no offer");

    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(OFFER).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.sdp(), Some(OFFER.as_bytes()), "the callee's offer reaches the caller");

    let mut dialog = call.ack_with(Some(ANSWER)).await;
    let ack = bob.receive("ACK").await;
    assert_eq!(ack.request().body().as_ref(), ANSWER.as_bytes(), "the caller's answer");

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "one call record");
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

const SESSION: &str = "v=0\r\no=alice 101 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
                       t=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const EARLY_OFFER: &str = "v=0\r\no=alice 102 1 IN IP4 127.0.0.1\r\ns=-\r\n\
                           c=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10004 RTP/AVP 0\r\n";
const EARLY_ANSWER: &str = "v=0\r\no=bob 501 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
                            t=0 0\r\nm=audio 20004 RTP/AVP 0\r\n";
const SESSION_ANSWER: &str = "v=0\r\no=bob 401 1 IN IP4 127.0.0.1\r\ns=-\r\n\
                              c=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const LATE_EARLY: &str = "v=0\r\no=alice 777 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
                          t=0 0\r\nm=audio 10008 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=bob 401 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
                       t=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=alice 101 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\n\
                        t=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";

/// The caller offers in a session part beside an early-session part (RFC 3959
/// §3); the callee's 183 carries a bare early-session answer, its 200 the
/// session answer, which reaches the caller as the callee wrote it.
#[tokio::test(start_paused = true)]
async fn an_early_session_answer_in_a_183_opens_no_session_for_the_caller() {
    let h = Harness::new("b2bua-early-session-183");
    let alice = h.agent("alice", "127.0.0.1:5472").await;
    let bob = h.agent("bob", "127.0.0.1:5482").await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", 5482).start(&h, "b2bua", "127.0.0.1:5492").await;

    let body = format!(
        "--b\r\nContent-Type: application/sdp\r\n\r\n{SESSION}\r\n--b\r\n\
         Content-Type: application/sdp\r\nContent-Disposition: early-session\r\n\r\n\
         {EARLY_OFFER}\r\n--b--\r\n"
    );
    let mut call = alice
        .invite(&bob)
        .with_body("multipart/mixed;boundary=b", body.into_bytes())
        .with_header("Supported", "early-session")
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    assert_eq!(uas.request().sdp(), Some(SESSION.as_bytes()));

    uas.respond(183, "Session Progress")
        .with_body("application/sdp", EARLY_ANSWER.as_bytes().to_vec())
        .with_header("Content-Disposition", "early-session")
        .await;
    let early = call.expect(183).await;
    assert_eq!(early.raw(HeaderName::ContentDisposition).next(), Some("early-session"));
    assert_eq!(early.body().as_ref(), EARLY_ANSWER.as_bytes(), "the early answer as written");

    uas.respond(200, "OK").with_sdp(SESSION_ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(ok.sdp(), Some(SESSION_ANSWER.as_bytes()), "the session answer as written");

    let mut dialog = call.ack_with(None).await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "one call record");
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// An established call: alice's dialog, bob's, and the SUT.
async fn established(
    h: &Harness,
    ports: (u16, u16, u16),
) -> (scenario_harness::Agent, scenario_harness::Agent, B2buaSut, Dialog, Dialog) {
    let (a, b, s) = ports;
    let alice = h.agent("alice", &format!("127.0.0.1:{a}")).await;
    let bob = h.agent("bob", &format!("127.0.0.1:{b}")).await;
    let b2bua =
        B2buaSut::route_all_to("127.0.0.1", b).start(h, "b2bua", &format!("127.0.0.1:{s}")).await;
    let mut call = alice.invite(&bob).with_sdp(SESSION).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(SESSION_ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    let bob_dialog = uas.dialog();
    bob.receive("ACK").await;
    (alice, bob, b2bua, dialog, bob_dialog)
}

/// Bob hangs up, and the call is gone with its record.
async fn bob_hangs_up(
    h: Harness,
    alice: &scenario_harness::Agent,
    b2bua: B2buaSut,
    mut bob_dialog: Dialog,
) {
    let mut bye = bob_dialog.bye().await;
    alice.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "one call record");
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// On an established call an UPDATE whose only body is a bare early-session
/// description reaches the callee as written and offers nothing.
#[tokio::test(start_paused = true)]
async fn an_update_early_session_body_rides_as_written() {
    let h = Harness::new("b2bua-early-session-update");
    let (alice, bob, b2bua, mut dialog, bob_dialog) = established(&h, (5473, 5483, 5493)).await;
    let mut update = dialog
        .send_request(InDialogMethod::Update)
        .with_body("application/sdp", LATE_EARLY.as_bytes().to_vec())
        .with_header("Content-Disposition", "early-session")
        .send()
        .await;
    let mut uas_update = bob.receive("UPDATE").await;
    assert_eq!(uas_update.request().body().as_ref(), LATE_EARLY.as_bytes(), "UPDATE as written");
    uas_update.respond(200, "OK").await;
    update.expect(200).await;
    bob_hangs_up(h, &alice, b2bua, bob_dialog).await;
}

/// On an established call a re-INVITE whose only body is a bare early-session
/// description reaches the callee as written and runs as a delayed offer.
#[tokio::test(start_paused = true)]
async fn a_reinvite_early_session_body_rides_as_written() {
    let h = Harness::new("b2bua-early-session-reinvite");
    let (alice, bob, b2bua, mut dialog, bob_dialog) = established(&h, (5474, 5484, 5494)).await;
    let mut reinvite = dialog
        .send_request(InDialogMethod::Invite)
        .with_body("application/sdp", LATE_EARLY.as_bytes().to_vec())
        .with_header("Content-Disposition", "early-session")
        .send_cancellable()
        .await;
    let mut uas_reinvite = bob.receive("INVITE").await;
    assert_eq!(
        uas_reinvite.request().body().as_ref(),
        LATE_EARLY.as_bytes(),
        "re-INVITE as written"
    );
    uas_reinvite.respond(200, "OK").with_sdp(REOFFER).await;
    let ok = reinvite.expect(200).await;
    assert_eq!(ok.sdp(), Some(REOFFER.as_bytes()), "the callee's offer");
    dialog.ack(Some(REANSWER)).await;
    let ack = bob.receive("ACK").await;
    assert_eq!(ack.request().body().as_ref(), REANSWER.as_bytes(), "the caller's answer");
    bob_hangs_up(h, &alice, b2bua, bob_dialog).await;
}
