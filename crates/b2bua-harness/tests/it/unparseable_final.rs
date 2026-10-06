//! A callee's failure final the B2BUA cannot use as sent.
//!
//! - A final of 300 or more names no dialog target, so an extra Contact on it
//!   is dropped and the final relayed at once (RFC 3261 §12.1.1 governs only
//!   the 1xx / 2xx that establish a dialog).
//! - A final the parser still refuses is discarded: it matches no client
//!   transaction (§17.1.3), the INVITE retransmits under Timer A, and Timer B
//!   ends the transaction as a timeout (§17.1.1.2). The call then fails over
//!   the ordinary timeout path, its CDR written and its state reaped.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaScene};
use sip_message::header::HeaderName;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// Bob's 486 carries his own Contact and a second one: the 486 reaches Alice
/// at once with Bob's `Warning`, and no Contact (a refusal names none).
#[tokio::test(start_paused = true)]
async fn a_failure_final_with_an_extra_contact_is_relayed() {
    let s = B2buaScene::new("failure-final-extra-contact").await;

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(486, "Busy Here")
        .with_header("Contact", "<sip:carol@10.0.0.9>")
        .with_header("Warning", "399 bob.example \"busy\"")
        .await;
    s.bob.receive("ACK").await;
    let busy = call.expect(486).await;
    assert!(busy.raw(HeaderName::Warning).next().is_some(), "Bob's Warning rides");
    assert!(busy.raw(HeaderName::Contact).next().is_none(), "a refusal names no Contact");

    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.h.finish().await;
}

/// Bob's 486 repeats its To header, which no reader accepts: the B2BUA drops
/// it, retransmits the INVITE, and at Timer B (64·T1) fails the call over the
/// timeout path. Alice gets a final, the call is reaped, and nothing hangs.
#[tokio::test(start_paused = true)]
async fn an_unparseable_failure_final_ends_the_call_at_timer_b() {
    let s = B2buaScene::new("unparseable-failure-final").await;

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let uas = s.bob.receive("INVITE").await;
    let req = uas.request();
    let via = req.raw(HeaderName::Via).next().expect("a Via").to_string();
    let from = req.raw(HeaderName::From).next().expect("a From").to_string();
    let to = req.raw(HeaderName::To).next().expect("a To").to_string();
    let call_id = req.call_id().to_string();
    let cseq = req.cseq().seq();
    let wire = format!(
        "SIP/2.0 486 Busy Here\r\n\
         Via: {via}\r\n\
         From: {from}\r\n\
         To: {to};tag=bob-486\r\n\
         To: {to};tag=bob-other\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} INVITE\r\n\
         Content-Length: 0\r\n\r\n"
    );
    s.bob.try_send_datagram(wire.as_bytes(), s.b2bua.addr).await.expect("the 486 leaves");

    // Timer B is 64·T1 = 32 s; past it the client transaction has timed out.
    s.h.advance(Duration::from_secs(33)).await;
    call.expect(408).await;

    // The Trying b-leg is CANCELed and stays unresolved, so the terminating
    // call waits on its safety timeout (32 s after the teardown began) and is
    // reaped at about 64 s; 35 s past the 408 leaves slack over that instant.
    s.h.advance(Duration::from_secs(35)).await;
    s.bob.drain().await;
    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    assert_eq!(s.b2bua.cdr_records().len(), 1, "the call's CDR is written");
    let _ = s.h.finish().await;
}
