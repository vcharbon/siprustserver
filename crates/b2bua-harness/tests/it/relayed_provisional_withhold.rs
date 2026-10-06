//! A call's own withhold on the provisional responses relayed toward the
//! originator (`features.withhold_on_relayed_provisionals`): a named header
//! stays behind on every relayed 1xx, whether relayed as received or masked
//! into a bare 180, and rides every other message; a call that withholds
//! nothing relays it.

use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallTreatment, ScriptedDecisionEngine};
use b2bua_harness::{B2buaScene, B2buaSut};
use call::features::{Relay18xMessages, RelayFirst18xStrategy, RelayFirst18xTo180Feature};
use sip_message::header::HeaderName;
use sip_message::SipResponse;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const PAI: &str = "P-Asserted-Identity";
const CALLEE_PAI: &str = "<sip:+15550002@op.example>";
/// An extension header the withhold does not name: it rides beside it.
const NOTE: &str = "X-Relay-Note";

fn lines(resp: &SipResponse, name: &str) -> Vec<String> {
    resp.raw(HeaderName::from(name)).map(str::to_string).collect()
}

/// A scene whose route withholds `withheld` from the relayed provisionals,
/// under `strategy` where one is armed.
async fn scene(
    name: &str,
    withheld: Option<Vec<String>>,
    strategy: Option<RelayFirst18xStrategy>,
) -> B2buaScene {
    B2buaScene::with_b2bua(name, move |bob_port| {
        B2buaSut::builder(Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(move |_| {
                    let mut r = route_to("127.0.0.1", bob_port);
                    r.features.withhold_on_relayed_provisionals = withheld.clone();
                    r.features.relay_first_18x_to_180 = strategy.map(|strategy| {
                        RelayFirst18xTo180Feature { strategy, messages: Relay18xMessages::All }
                    });
                    CallTreatment::Route(r)
                })
                .build(),
        ))
        .tune(|c| c.privacy_service = false)
    })
    .await
}

/// Bob sends a 180 and a 183, then answers, each message stating his identity
/// and the note; alice is shown each provisional as a 180 where `masked`.
/// Returns what alice was shown of both on each: (status, identity, note).
async fn ring_twice_then_answer(
    s: &B2buaScene,
    masked: bool,
) -> Vec<(u16, Vec<String>, Vec<String>)> {
    let mut seen = Vec::new();
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    for (status, reason) in [(180, "Ringing"), (183, "Session Progress")] {
        uas.respond(status, reason).with_header(PAI, CALLEE_PAI).with_header(NOTE, "1xx").await;
        let shown = call.expect(if masked { 180 } else { status }).await;
        seen.push((status, lines(&shown, PAI), lines(&shown, NOTE)));
    }
    uas.respond(200, "OK")
        .with_sdp(ANSWER)
        .with_header(PAI, CALLEE_PAI)
        .with_header(NOTE, "200")
        .await;
    let ok = call.expect(200).await;
    seen.push((200, lines(&ok, PAI), lines(&ok, NOTE)));
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    s.hangup(&mut dialog).await;
    seen
}

/// The named header stays behind on each relayed provisional and rides the
/// answer; the unnamed note rides everywhere.
#[tokio::test(start_paused = true)]
async fn the_withheld_header_stays_behind_on_every_relayed_provisional() {
    let s = scene("provisional-withhold", Some(vec![PAI.to_string()]), None).await;
    for (status, identity, note) in ring_twice_then_answer(&s, false).await {
        let expected: Vec<String> =
            if status < 200 { Vec::new() } else { vec![CALLEE_PAI.to_string()] };
        assert_eq!(identity, expected, "{PAI} on the relayed {status}");
        assert_eq!(note.len(), 1, "{NOTE} rides on the relayed {status}");
    }
    let _report = s.finish().await;
}

/// The same withhold holds on the bare 180s a masking strategy shows the
/// originator, which is matched case-insensitively by name.
#[tokio::test(start_paused = true)]
async fn the_withhold_holds_on_the_masked_180() {
    let s = scene(
        "provisional-withhold-masked",
        Some(vec!["p-asserted-identity".to_string()]),
        Some(RelayFirst18xStrategy::DropSdp),
    )
    .await;
    for (status, identity, _) in ring_twice_then_answer(&s, true).await {
        if status < 200 {
            assert!(identity.is_empty(), "{PAI} stays off the masked 180: {identity:?}");
        } else {
            assert_eq!(identity, [CALLEE_PAI], "{PAI} rides on the relayed 200");
        }
    }
    let _report = s.finish().await;
}

/// A call that withholds nothing relays the header on every provisional.
#[tokio::test(start_paused = true)]
async fn a_call_withholding_nothing_relays_it() {
    let s = scene("provisional-withhold-none", None, None).await;
    for (status, identity, _) in ring_twice_then_answer(&s, false).await {
        assert_eq!(identity, [CALLEE_PAI], "{PAI} on the relayed {status}");
    }
    let _report = s.finish().await;
}
