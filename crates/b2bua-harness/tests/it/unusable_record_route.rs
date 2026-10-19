//! A callee whose 2xx records a route no request can be addressed by: a
//! Record-Route URI that reads as a name-addr but fails the Request-URI checks
//! (a port with trailing garbage, no host, a control byte). The B2BUA keeps no
//! such route in the dialog's route set (RFC 3261 §12.1.2): the ACK and the
//! BYE still reach the callee at its Contact, and the call ends recorded and
//! reaped.

use b2bua_harness::{settle_until, B2buaScene};
use call::CdrEventType;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// Alice calls bob, bob answers recording `route`, alice hangs up.
async fn call_recording(name: &str, route: &str) {
    let s = B2buaScene::new(name).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).with_header("Record-Route", route).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;

    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    let cdrs = s.b2bua.cdr_records();
    let kinds: Vec<CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(kinds.contains(&CdrEventType::Answer), "answer: {kinds:?}");
    assert!(kinds.contains(&CdrEventType::Bye), "bye: {kinds:?}");
    let _report = s.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_recorded_port_with_trailing_garbage_routes_nothing() {
    call_recording("b2bua-rr-port-garbage", "<sip:10.0.0.9:5060x>").await;
}

#[tokio::test(start_paused = true)]
async fn a_recorded_route_with_no_host_routes_nothing() {
    call_recording("b2bua-rr-no-host", "<sip:@>").await;
}

#[tokio::test(start_paused = true)]
async fn a_recorded_route_with_a_control_byte_routes_nothing() {
    call_recording("b2bua-rr-control-byte", "<sip:10.0.0.\u{7f}9>").await;
}
