//! What the stack stored reads back whatever the process read the wire under: a
//! deployment stating a larger line limit than the default still CANCELs through
//! to a ringing callee, though the INVITE as it left is past the default reader
//! (RFC 3261 §9.1 builds the CANCEL from it).

use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{B2buaScene, B2buaSut};
use sip_message::{CustomParser, FirstIngressBound, SipParser, SipParserLimits};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
/// More header lines than the default reader takes.
const LINES: usize = 200;

fn larger() -> SipParserLimits {
    SipParserLimits {
        max_header_lines: LINES + 32,
        first_ingress: FirstIngressBound { header_lines: LINES, ..FirstIngressBound::DEFAULT },
        ..SipParserLimits::DEFAULT
    }
}

#[tokio::test(start_paused = true)]
async fn a_cancel_reaches_a_ringing_callee_past_the_default_reader() {
    let s = B2buaScene::with_b2bua("stored-image-cancel", |bob_port| {
        let engine = ScriptedDecisionEngine::builder()
            .fallback(move |_| NewCallResponse::Route(route_to("127.0.0.1", bob_port)))
            .build();
        B2buaSut::builder(Arc::new(engine)).tune(|config| config.parser_limits = larger())
    })
    .await;
    s.bob.reads_with(sip_message::CustomParser::with_limits(larger()));

    let mut invite = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr);
    for n in 0..150 {
        invite = invite.with_header(&format!("X-Pad-{n}"), "p");
    }
    let mut call = invite.send().await;
    let mut uas = s.bob.receive("INVITE").await;
    assert!(
        CustomParser::new().parse(uas.request().image()).is_err(),
        "the default reader refuses the INVITE as it left"
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
