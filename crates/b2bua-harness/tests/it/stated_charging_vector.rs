//! RFC 7315 §5.6 — a decision that states the call's charging vector has it
//! carried on every message the B2BUA sends on every leg of the call: the
//! originated INVITE, the provisional and final toward the originator, in-dialog
//! requests both ways and their responses, the ACK and the BYE. `100 Trying` is
//! hop-by-hop (RFC 3261 §21.1.1) and carries none. The originator's own vector
//! and the originated-leg arm's mint give way to the stated one.
//!
//! A repeated 2xx is the response it repeats (RFC 3261 §13.3.1.4), so both
//! un-ACKed-2xx ladders — the originator's answer and the in-dialog re-INVITE
//! 2xx — repeat the stated vector byte for byte, as does the re-ACK of a
//! repeated 2xx (§13.2.2.4).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{B2buaScene, B2buaSut};
use call::features::ChargingVectorFeature;
use scenario_harness::RunReport;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const BOB_REOFFER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";
const ALICE_REANSWER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";

const STATED: &str = "icid-value=call-0001;icid-generated-at=as.example.net;orig-ioi=example.net";
const ORIGINATOR_OWN: &str = "icid-value=edge-0001;icid-generated-at=edge.example.com";

/// Longer than T1 (500 ms) and well inside the 32 s give-up: exactly one ladder
/// rung fires while the ACK is held.
const HELD_ACK: Duration = Duration::from_millis(700);

async fn scene() -> B2buaScene {
    B2buaScene::with_b2bua("stated-charging-vector", |bob_port| {
        let engine = ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut route = route_to("127.0.0.1", bob_port);
                route.features.charging_vector = Some(ChargingVectorFeature::default());
                route.features.stated_charging_vector = Some(STATED.to_string());
                NewCallResponse::Route(route)
            })
            .build();
        B2buaSut::builder(Arc::new(engine))
    })
    .await
}

/// The `(start line, CSeq, charging lines)` of every datagram `from` sent `to`.
fn sent(
    report: &RunReport,
    from: SocketAddr,
    to: SocketAddr,
) -> Vec<(String, String, Vec<String>)> {
    report
        .entries()
        .iter()
        .filter(|e| e.from == from && e.to == to)
        .map(|e| {
            let text = String::from_utf8_lossy(&e.raw).to_string();
            let mut lines = text.split("\r\n");
            let start = lines.next().unwrap_or_default().to_string();
            let headers: Vec<&str> = lines.take_while(|l| !l.is_empty()).collect();
            let value = |name: &str| {
                headers
                    .iter()
                    .filter_map(|l| l.split_once(':'))
                    .filter(|(n, _)| n.trim().eq_ignore_ascii_case(name))
                    .map(|(_, v)| v.trim().to_string())
                    .collect::<Vec<_>>()
            };
            (start, value("CSeq").join(","), value("P-Charging-Vector"))
        })
        .collect()
}

/// The datagrams `from` sent `to` whose start line and CSeq method match, in
/// send order.
fn copies(
    report: &RunReport,
    from: SocketAddr,
    to: SocketAddr,
    start: &str,
    method: &str,
) -> Vec<Vec<u8>> {
    report
        .entries()
        .iter()
        .filter(|e| e.from == from && e.to == to && e.raw.starts_with(start.as_bytes()))
        .filter(|e| {
            String::from_utf8_lossy(&e.raw)
                .split("\r\n")
                .any(|l| l.to_ascii_lowercase().starts_with("cseq:") && l.ends_with(method))
        })
        .map(|e| e.raw.clone())
        .collect()
}

#[track_caller]
fn assert_every_message_states(who: &str, messages: &[(String, String, Vec<String>)]) {
    assert!(!messages.is_empty(), "{who}: nothing sent");
    for (start, cseq, lines) in messages {
        let expected: Vec<String> =
            if start.starts_with("SIP/2.0 100 ") { vec![] } else { vec![STATED.to_string()] };
        assert_eq!(lines, &expected, "{who}: `{start}` ({cseq}) — every message: {messages:#?}");
    }
}

#[track_caller]
fn assert_repeated_whole(what: &str, copies: &[Vec<u8>]) {
    assert!(copies.len() >= 2, "{what}: repeated, got {} copies", copies.len());
    for copy in &copies[1..] {
        assert_eq!(copy, &copies[0], "{what}: a repeat is the message it repeats");
    }
}

#[tokio::test(start_paused = true)]
async fn a_stated_charging_vector_rides_every_message_of_the_call() {
    let s = scene().await;

    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("P-Charging-Vector", ORIGINATOR_OWN)
        .delayed_ack(HELD_ACK)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    // The caller holds her ACK past T1: the answer's ladder repeats it.
    let _alice_dialog = call.ack_delayed().await;
    s.bob.receive("ACK").await;
    s.alice.drain().await;

    // The callee repeats its 2xx as though the ACK was lost: the retained ACK
    // is re-passed (RFC 3261 §13.2.2.4).
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let mut re_acks = 0;
    for _ in 0..10 {
        s.h.advance(Duration::from_millis(100)).await;
        re_acks += s.bob.drain().await;
        if re_acks >= 1 {
            break;
        }
    }
    assert_eq!(re_acks, 1, "the repeated 2xx is re-ACKed");

    // The callee re-INVITEs and holds its ACK past T1: the re-INVITE 2xx's
    // ladder repeats the caller's answer toward it.
    let mut bob_dialog = uas.dialog();
    let mut reinv = bob_dialog.reinvite(Some(BOB_REOFFER)).await;
    s.alice.receive("INVITE").await.respond(200, "OK").with_sdp(ALICE_REANSWER).await;
    reinv.expect(200).await;
    tokio::time::sleep(HELD_ACK).await;
    s.bob.drain().await;
    bob_dialog.ack(None).await;
    s.alice.receive("ACK").await;

    let mut bye = bob_dialog.bye().await;
    s.alice.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let (b2bua, alice, bob) = (s.b2bua.addr, s.alice.addr(), s.bob.addr());
    let report = s.finish().await;

    assert_every_message_states("toward the originator", &sent(&report, b2bua, alice));
    assert_every_message_states("toward the originated leg", &sent(&report, b2bua, bob));
    assert_repeated_whole("the answer", &copies(&report, b2bua, alice, "SIP/2.0 200 ", "INVITE"));
    let reinvite_2xx: Vec<Vec<u8>> = copies(&report, b2bua, bob, "SIP/2.0 200 ", "INVITE");
    assert_repeated_whole("the re-INVITE 2xx", &reinvite_2xx);
    assert_repeated_whole("the re-ACK", &copies(&report, b2bua, bob, "ACK ", "ACK"));
}
