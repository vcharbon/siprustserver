//! Where the B2BUA's own `Contact` rides an a-facing response (RFC 3261
//! Table 3): a 1xx keeps the early dialog reachable, a 2xx to INVITE must
//! carry one, a 3xx/485 names where to retry. Every other final ends the
//! transaction and names no reachable dialog, so it carries none — a Contact
//! there is noise the caller cannot use and leaks the call reference.

use b2bua_harness::{settle_until, B2buaScene};
use sip_message::generators::InDialogMethod;
use sip_message::header::HeaderName;
use sip_message::types::SipResponse;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30001 RTP/AVP 0\r\n";

fn contacts(resp: &SipResponse) -> Vec<String> {
    resp.raw(HeaderName::Contact).map(str::to_string).collect()
}

/// Initial INVITE: the ring reaches the caller with the B2BUA's Contact (she
/// may address the early dialog), the refusal that ends the transaction
/// carries none.
#[tokio::test]
async fn the_ring_names_the_b2bua_and_the_refusal_names_nothing() {
    let s = B2buaScene::new("contact-scope-initial-refusal").await;

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    let ringing = call.expect(180).await;
    assert_eq!(contacts(&ringing).len(), 1, "the early dialog is reachable at the B2BUA");

    uas.respond(486, "Busy Here").await;
    s.bob.receive("ACK").await; // the b2bua completes bob's reject txn (§17.1.1.3)
    let busy = call.expect(486).await;
    assert_eq!(contacts(&busy), Vec::<String>::new(), "a refusal names no reachable dialog");

    settle_until(|| s.b2bua.active_calls() == 0).await;
    s.b2bua.assert_fully_reaped();
    let _report = s.finish().await;
}

/// In-dialog: the relayed re-INVITE failure carries no Contact, while the 2xx
/// that renegotiates the session still carries the target the caller addresses
/// (RFC 3261 §12.2.1.2).
#[tokio::test]
async fn a_relayed_reinvite_answer_names_the_target_and_its_failure_does_not() {
    let s = B2buaScene::new("contact-scope-reinvite").await;
    let mut dialog = s.establish().await;

    // ── alice re-INVITEs; bob refuses ──
    let mut reinv = dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    s.bob.receive("INVITE").await.respond(488, "Not Acceptable Here").await;
    let refused = reinv.expect(488).await;
    assert_eq!(
        contacts(&refused),
        Vec::<String>::new(),
        "a failed renegotiation names no new target"
    );
    s.alice.drain().await;
    s.bob.drain().await;

    // ── alice re-INVITEs again; bob answers ──
    let mut reinv = dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    s.bob.receive("INVITE").await.respond(200, "OK").with_sdp(REANSWER).await;
    let ok = reinv.expect(200).await;
    assert_eq!(contacts(&ok).len(), 1, "the answered renegotiation restates the remote target");
    dialog.ack(None).await;
    s.bob.receive("ACK").await;

    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.active_calls() == 0).await;
    s.b2bua.assert_fully_reaped();
    let _report = s.finish().await;
}

/// The a-leg 200 the B2BUA mints on the answered call carries its Contact —
/// the gate keeps what RFC 3261 §13.3.1.4 makes mandatory.
#[tokio::test]
async fn the_answer_carries_the_contact_the_caller_addresses() {
    let s = B2buaScene::new("contact-scope-answer").await;

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let ok = call.expect(200).await;
    assert_eq!(contacts(&ok).len(), 1, "the confirmed dialog is addressed at the B2BUA");
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;

    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.active_calls() == 0).await;
    s.b2bua.assert_fully_reaped();
    let _report = s.finish().await;
}
