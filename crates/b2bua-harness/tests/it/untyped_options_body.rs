//! A 200 to OPTIONS whose body states no Content-Type (RFC 3261 §20.15), the
//! shape `options-body-no-type` exempts, holds no body this stack can type: the
//! 200 relayed in-dialog carries no body, never one typed by a default.

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{EmitOpts, Harness, MessageTemplate, WaiverScope};
use sip_message::generators::InDialogMethod;
use sip_message::header::HeaderName;
use sip_message::Exemptions;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\n";

#[tokio::test(start_paused = true)]
async fn an_untyped_options_200_body_is_not_relayed() {
    let h = Harness::with_transit_delay("options-200-untyped-body", 1);
    h.waive(
        WaiverScope::rule(
            "content-type",
            "bob answers OPTIONS with an untyped body, the peer shape the deployment exempts",
        )
        .on_party("bob"),
    );
    let alice = h.agent_auto("alice").await;
    let bob = h.agent_auto("bob").await;
    let b2bua = B2buaSut::route_all_to(&bob.addr().ip().to_string(), bob.addr().port())
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.parser_limits.exemptions =
                Exemptions::read(Some("options-body-no-type")).expect("a name");
        })
        .start(&h, "b2bua", &h.next_addr().to_string())
        .await;
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    let mut options = dialog.send_request(InDialogMethod::Options).send().await;
    let untyped = MessageTemplate::response(200, "OK", Vec::new(), b"opaque".to_vec());
    bob.receive("OPTIONS").await.respond(200, "OK").template(&untyped, EmitOpts::default()).await;
    let ok = options.expect(200).await;
    assert!(ok.body().is_empty(), "no body relayed: {:?}", String::from_utf8_lossy(ok.body()));
    assert_eq!(ok.raw(HeaderName::ContentType).next(), None, "no type invented");

    dialog.bye().await.expect(200).await;
    bob.receive("BYE").await.respond(200, "OK").await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
