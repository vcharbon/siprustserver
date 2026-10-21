//! A callee whose 2xx records a route no request can be addressed by: a
//! Record-Route URI the grammar refuses (a port with trailing garbage, no
//! host, a control byte). The parser refuses the 2xx (RFC 3261 §20.30, §25.1),
//! so it matches no client transaction (§17.1.3): the INVITE retransmits under
//! Timer A and Timer B ends the call as a timeout (§17.1.1.2), its CDR written
//! and its state reaped. No such route ever becomes a dialog's route set.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaScene};
use scenario_harness::WaiverScope;
use sip_message::header::HeaderName;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// Alice calls bob, bob's 200 records `route`: the B2BUA drops it and fails
/// the call at Timer B.
async fn call_recording(name: &str, route: &str) {
    let s = B2buaScene::new(name).await;
    // The audit reads the 2xx without the wire grammar, so it sees a dialog
    // the B2BUA's parser refused: the 2xx (wire entry 4) is the input under
    // test, and its ACK and BYE are owed by nobody.
    s.h.waive(
        WaiverScope::rule(
            "no-ack-to-dialog-creating-2xx",
            "the 2xx records a route the grammar refuses; the B2BUA's parser discards it (RFC 3261 §25.1)",
        )
        .on_party("b2bua")
        .at_position(4),
    );
    s.h.waive(
        WaiverScope::rule(
            "unacked-2xx-not-cleared",
            "bob deliberately sends one malformed 2xx and nothing after it — the input under test",
        )
        .on_party("bob")
        .at_position(4),
    );
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let uas = s.bob.receive("INVITE").await;
    let req = uas.request();
    let via = req.raw(HeaderName::Via).next().expect("a Via").to_string();
    let from = req.raw(HeaderName::From).next().expect("a From").to_string();
    let to = req.raw(HeaderName::To).next().expect("a To").to_string();
    let call_id = req.call_id().to_string();
    let cseq = req.cseq().seq();
    let wire = format!(
        "SIP/2.0 200 OK\r\n\
         Via: {via}\r\n\
         Record-Route: {route}\r\n\
         From: {from}\r\n\
         To: {to};tag=bob-200\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} INVITE\r\n\
         Contact: <sip:bob@127.0.0.1>\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {}\r\n\r\n{ANSWER}",
        ANSWER.len()
    );
    s.bob.try_send_datagram(wire.as_bytes(), s.b2bua.addr).await.expect("the 200 leaves");

    // Timer B is 64·T1 = 32 s; past it the client transaction has timed out.
    s.h.advance(Duration::from_secs(33)).await;
    call.expect(408).await;

    // The Trying b-leg is CANCELed and stays unresolved, so the terminating
    // call waits on its safety timeout and is reaped at about 64 s.
    s.h.advance(Duration::from_secs(35)).await;
    s.bob.drain().await;
    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    assert_eq!(s.b2bua.cdr_records().len(), 1, "the call's CDR is written");
    let _ = s.h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_recorded_port_with_trailing_garbage_refuses_the_2xx() {
    call_recording("b2bua-rr-port-garbage", "<sip:10.0.0.9:5060x>").await;
}

#[tokio::test(start_paused = true)]
async fn a_recorded_route_with_no_host_refuses_the_2xx() {
    call_recording("b2bua-rr-no-host", "<sip:@>").await;
}

#[tokio::test(start_paused = true)]
async fn a_recorded_route_with_a_control_byte_refuses_the_2xx() {
    call_recording("b2bua-rr-control-byte", "<sip:10.0.0.\u{7f}9>").await;
}
