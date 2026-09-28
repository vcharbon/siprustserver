//! RFC 7315 §5.6 — a decision that states the call's charging vector has it
//! carried on every message the B2BUA sends on every leg of the call: the
//! originated INVITE, the provisional and final toward the originator, in-dialog
//! requests both ways and their responses, the ACK and the BYE. `100 Trying` is
//! hop-by-hop (RFC 3261 §21.1.1) and is left as it is. The originator's own
//! vector and the originated-leg arm's mint give way to the stated one; a stated
//! removal takes every copy off and mints none.
//!
//! A repeat is the message it repeats (RFC 3261 §13.3.1.4, §13.2.2.4; RFC 3262
//! §3), so the un-ACKed 2xx ladders on both faces, the re-ACK of a repeated 2xx
//! and the un-PRACKed reliable provisional ladder repeat the stated vector byte
//! for byte.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{advance, B2buaScene, B2buaSut};
use call::features::{ChargingVectorFeature, StatedChargingVector};
use scenario_harness::RunReport;
use sip_message::header::HeaderName;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const BOB_REOFFER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";
const ALICE_REANSWER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";

const STATED: &str = "icid-value=call-0001;icid-generated-at=as.example.net;orig-ioi=example.net";
const ORIGINATOR_OWN: &str = "icid-value=edge-0001;icid-generated-at=edge.example.com";
const ANSWERER_OWN: &str = "icid-value=far-0001;term-ioi=far.example.org";

/// Longer than T1 (500 ms) and well inside the 32 s give-up: exactly one ladder
/// rung fires while the ACK is held.
const HELD_ACK: Duration = Duration::from_millis(700);

/// The scene whose every route states `statement` beside the originated-leg
/// minting arm.
async fn scene(name: &str, statement: StatedChargingVector) -> B2buaScene {
    B2buaScene::with_b2bua(name, |bob_port| {
        let engine = ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut route = route_to("127.0.0.1", bob_port);
                route.features.charging_vector = Some(ChargingVectorFeature::default());
                route.features.stated_charging_vector = Some(statement.clone());
                NewCallResponse::Route(route)
            })
            .build();
        B2buaSut::builder(Arc::new(engine))
    })
    .await
}

fn stated() -> StatedChargingVector {
    StatedChargingVector::Lines(vec![STATED.to_string()])
}

/// One datagram the SUT sent: its start line, CSeq, charging lines and bytes.
struct Sent {
    start: String,
    cseq: String,
    status: Option<u16>,
    lines: Vec<String>,
    raw: Vec<u8>,
}

/// Every datagram `from` sent `to`, read with the message parser.
fn sent(report: &RunReport, from: SocketAddr, to: SocketAddr) -> Vec<Sent> {
    let name = HeaderName::from("P-Charging-Vector");
    report
        .entries()
        .iter()
        .filter(|e| e.from == from && e.to == to)
        .map(|e| {
            let msg = CustomParser::new().parse(&e.raw).expect("the SUT sends parseable SIP");
            let start = String::from_utf8_lossy(&e.raw).lines().next().unwrap_or("").to_string();
            let (cseq, status, lines) = match &msg {
                SipMessage::Request(r) => (
                    format!("{} {}", r.cseq().seq(), r.cseq().method().as_str()),
                    None,
                    r.raw(name.clone()).map(str::to_string).collect(),
                ),
                SipMessage::Response(r) => (
                    format!("{} {}", r.cseq().seq(), r.cseq().method().as_str()),
                    Some(r.status()),
                    r.raw(name.clone()).map(str::to_string).collect(),
                ),
            };
            Sent { start, cseq, status, lines, raw: e.raw.clone() }
        })
        .collect()
}

/// Every message but `100 Trying` carries exactly `expected`.
#[track_caller]
fn assert_every_message_carries(who: &str, messages: &[Sent], expected: &[&str]) {
    assert!(!messages.is_empty(), "{who}: nothing sent");
    for m in messages.iter().filter(|m| m.status != Some(100)) {
        assert_eq!(m.lines, expected, "{who}: `{}` ({})", m.start, m.cseq);
    }
}

/// The copies of one message among `messages`: same start-line prefix and CSeq.
fn copies<'a>(messages: &'a [Sent], start: &str, cseq: &str) -> Vec<&'a [u8]> {
    messages
        .iter()
        .filter(|m| m.start.starts_with(start) && m.cseq == cseq)
        .map(|m| m.raw.as_slice())
        .collect()
}

#[track_caller]
fn assert_repeated_whole(what: &str, copies: &[&[u8]]) {
    assert!(copies.len() >= 2, "{what}: repeated, got {} copies", copies.len());
    for copy in &copies[1..] {
        assert_eq!(copy, &copies[0], "{what}: a repeat is the message it repeats");
    }
}

#[tokio::test(start_paused = true)]
async fn a_stated_charging_vector_rides_every_message_of_the_call() {
    let s = scene("stated-charging-vector", stated()).await;

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

    let to_alice = sent(&report, b2bua, alice);
    let to_bob = sent(&report, b2bua, bob);
    assert_every_message_carries("toward the originator", &to_alice, &[STATED]);
    assert_every_message_carries("toward the originated leg", &to_bob, &[STATED]);
    assert_repeated_whole("the answer", &copies(&to_alice, "SIP/2.0 200 ", "1 INVITE"));
    let reinvite_cseq = to_bob
        .iter()
        .find(|m| m.status == Some(200) && m.cseq.ends_with(" INVITE"))
        .map(|m| m.cseq.clone())
        .expect("the re-INVITE 2xx toward the callee");
    assert_repeated_whole("the re-INVITE 2xx", &copies(&to_bob, "SIP/2.0 200 ", &reinvite_cseq));
    assert_repeated_whole("the re-ACK", &copies(&to_bob, "ACK ", "1 ACK"));
}

/// The caller offers `100rel`, the callee answers with a reliable 183 and the
/// caller holds her PRACK across two rungs: every copy of the provisional the
/// B2BUA repeats (RFC 3262 §3) is the stamped one, byte for byte.
#[tokio::test(start_paused = true)]
async fn the_reliable_provisional_ladder_repeats_the_stated_vector() {
    let s = scene("stated-charging-vector-100rel", stated()).await;

    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(183, "Session Progress")
        .with_header("Require", "100rel")
        .with_header("RSeq", "4711")
        .with_sdp(ANSWER)
        .await;
    let p183 = call.expect(183).await;
    advance(1_600).await;
    s.alice.drain().await;

    let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the reliable 183");
    s.bob.receive("PRACK").await.respond(200, "OK").await;
    prack.expect(200).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let (b2bua, alice) = (s.b2bua.addr, s.alice.addr());
    let report = s.finish().await;
    let to_alice = sent(&report, b2bua, alice);
    assert_every_message_carries("toward the originator", &to_alice, &[STATED]);
    let rungs = copies(&to_alice, "SIP/2.0 183 ", "1 INVITE");
    assert!(rungs.len() >= 3, "two rungs fired: {} copies", rungs.len());
    assert_repeated_whole("the reliable 183", &rungs);
}

/// A stated removal: the originator's vector and the answerer's are taken off
/// every message, and the originated-leg arm mints none.
#[tokio::test(start_paused = true)]
async fn a_stated_removal_takes_every_copy_off_and_mints_none() {
    let s = scene("stated-charging-vector-removed", StatedChargingVector::Removed).await;

    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("P-Charging-Vector", ORIGINATOR_OWN)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_header("P-Charging-Vector", ANSWERER_OWN).with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let (b2bua, alice, bob) = (s.b2bua.addr, s.alice.addr(), s.bob.addr());
    let report = s.finish().await;
    assert_every_message_carries("toward the originator", &sent(&report, b2bua, alice), &[]);
    assert_every_message_carries("toward the originated leg", &sent(&report, b2bua, bob), &[]);
}
