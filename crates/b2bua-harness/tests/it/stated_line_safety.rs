//! A decision's stated header line that the wire cannot carry whole — a CR or
//! LF in its value, a name that is not a `token` (RFC 3261 §7.3, §25.1) — is
//! skipped, never written: it would split into a header line nobody stated.
//! The call's other statements ride every message, and each skip is a model
//! defect the run reports.

use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{HeaderUpdate, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use call::features::StatedHeaders;
use sip_message::header::HeaderName;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The stated line that would start a header line of its own.
const SPLITTING: &str = "ok\r\nVia: SIP/2.0/UDP 192.0.2.66:5060;branch=z9hG4bKinjected";

/// The model defect a skipped statement is counted under.
const DEFECT: &str = "stated-text-fits-its-position";

fn set(v: &[&str]) -> HeaderUpdate {
    HeaderUpdate::Set(v.iter().map(|l| l.to_string()).collect())
}

/// The scene whose every route states `stated`.
async fn scene(name: &str, stated: StatedHeaders) -> B2buaScene {
    B2buaScene::with_b2bua(name, |bob_port| {
        let engine = ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut route = route_to("127.0.0.1", bob_port);
                route.features.stated_headers = Some(stated.clone());
                NewCallResponse::Route(route)
            })
            .build();
        B2buaSut::builder(Arc::new(engine))
    })
    .await
}

/// The lines of `name` `msg` carries, in order.
fn lines(msg: &SipMessage, name: &str) -> Vec<String> {
    msg.raw(HeaderName::from(name)).map(str::to_string).collect()
}

fn is_trying(msg: &SipMessage) -> bool {
    matches!(msg, SipMessage::Response(r) if r.status() == 100)
}

#[tokio::test(start_paused = true)]
async fn a_stated_line_with_a_line_break_is_skipped_and_counted() {
    let mut stated = StatedHeaders::default();
    stated.every_message.insert("X-Split".into(), set(&[SPLITTING]));
    stated.every_message.insert("X-Kept".into(), set(&["fine"]));
    let mut s = scene("stated-line-unwritable", stated).await;

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    s.bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;

    let defects = s.b2bua.take_model_violations();
    let counted = s.b2bua.metrics().model_defects_total(DEFECT);
    let b2bua = s.b2bua.addr;
    let report = s.finish().await;

    let entries = report.entries();
    let sent: Vec<_> = entries.iter().filter(|e| e.from == b2bua).collect();
    assert!(!sent.is_empty(), "the call sent messages");
    for entry in sent {
        let text = String::from_utf8_lossy(&entry.raw);
        assert!(!text.contains("z9hG4bKinjected"), "the split reached the wire: {text}");
        let msg = CustomParser::new().parse(&entry.raw).expect("the SUT sends parseable SIP");
        assert!(lines(&msg, "X-Split").is_empty(), "{text}");
        let kept: &[&str] = if is_trying(&msg) { &[] } else { &["fine"] };
        assert_eq!(lines(&msg, "X-Kept"), kept, "{text}");
    }
    assert!(!defects.is_empty(), "the skip is reported");
    for defect in &defects {
        assert!(defect.starts_with(&format!("model defect {DEFECT}")), "{defect}");
    }
    assert!(counted >= 1, "the skip is counted");
}
