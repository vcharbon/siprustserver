//! RFC 3261 §19.1.1: "any given parameter-name MUST NOT appear more than
//! once". A deployment that names the shape (ADR-0001, exemptions) takes a
//! caller's From and Contact URIs written with a parameter twice, one value
//! each time, and the stack's own requests in that dialog carry one copy:
//! §12.2.1.1 puts the remote URI in their To and the remote target in their
//! Request-URI, and §19.1.4 compares a parameter by value, so the caller's
//! dialog still matches.

use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{B2buaScene, B2buaSut};
use sip_message::generators::InDialogMethod;
use sip_message::header::HeaderName;
use sip_message::{Exemptions, SipParserLimits};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// The caller's From and Contact, each URI writing one parameter twice.
const FROM: &str = "<sip:alice@example.com;x-dup=1;x-dup=1>";
const CONTACT: &str = "sip:alice@127.0.0.1:5060;x-two=a;x-two=a";

/// The reading of a deployment naming both shapes.
fn naming() -> SipParserLimits {
    let named = "repeat:from.uri:x-dup,repeat:contact.uri:x-two";
    let exemptions = Exemptions::read(Some(named)).expect("the names read");
    SipParserLimits { exemptions, ..SipParserLimits::default() }
}

async fn scene(name: &str) -> B2buaScene {
    let s = B2buaScene::with_b2bua(name, |bob_port| {
        let engine = ScriptedDecisionEngine::builder()
            .fallback(move |_| NewCallResponse::Route(route_to("127.0.0.1", bob_port)))
            .build();
        B2buaSut::builder(Arc::new(engine)).tune(|config| config.parser_limits = naming())
    })
    .await;
    // The responses the stack echoes her own From in reach her as she wrote it
    // (§8.2.6.2); she reads them as the deployment does.
    s.alice.reads_under(naming());
    s
}

/// The callee hangs up: the BYE the stack sends the caller names her in its To
/// and her target in its Request-URI, each with one copy of the parameter.
#[tokio::test(start_paused = true)]
async fn the_stacks_bye_names_the_caller_with_one_copy_of_a_repeated_parameter() {
    let s = scene("repeated-uri-parameter-bye").await;
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .from(FROM)
        .with_contact(CONTACT)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    let at_bob = uas.request().raw(HeaderName::From).next().unwrap_or_default().to_string();
    assert_eq!(at_bob.matches("x-dup=").count(), 1, "the b-leg From: {at_bob}");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    call.ack().await;
    s.bob.receive("ACK").await;

    let mut bye = uas.dialog().request(InDialogMethod::Bye, None).await;
    let mut at_alice = s.alice.receive("BYE").await;
    let to = at_alice.request().raw(HeaderName::To).next().unwrap_or_default().to_string();
    let target = at_alice.request().request_uri().to_string();
    at_alice.respond(200, "OK").await;
    bye.expect(200).await;
    assert_eq!(to.matches("x-dup=").count(), 1, "the stack's own To: {to}");
    assert_eq!(target.matches("x-two=").count(), 1, "the stack's own Request-URI: {target}");
    s.finish().await;
}
