//! What the stack stored reads back whatever bound the wire was read under: a
//! caller whose From fits the worker's bound but not the default one is still
//! CANCELled through to a ringing callee. The CANCEL is built from the INVITE as
//! it left, which carries that From (RFC 3261 §9.1).

use b2bua_harness::B2buaScene;
use sip_message::{CustomParser, SipParser, SipParserLimits};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

#[tokio::test(start_paused = true)]
async fn a_cancel_reaches_a_ringing_callee_past_the_default_line_bound() {
    let s = B2buaScene::new("stored-image-cancel").await;
    let worker = SipParserLimits::default().behind_proxy();
    s.alice.reads_under(worker.clone());
    s.bob.reads_under(worker);

    let from = format!("sip:{}@example.com", "a".repeat(8_200));
    let mut call =
        s.alice.invite(&s.bob).from(from).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    assert!(
        CustomParser::new().parse(uas.request().image()).is_err(),
        "the default bound refuses the INVITE as it left"
    );
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut cancel = call.cancel().await;
    cancel.expect(200).await;
    s.bob.receive("CANCEL").await.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    call.expect(487).await;

    s.finish().await;
}
