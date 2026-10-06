//! A decision's stated headers (`FeatureActivations::stated_headers`): one set
//! per scope, each applied to every message of its scope the B2BUA sends,
//! resolved against the message as built.
//!
//! - `every_message`: every request and response on the originator's leg and
//!   on every leg the call originates, both directions, the stack's own
//!   requests included;
//! - `originator_finals`: every final to the originator's initial INVITE;
//! - `launched_invite`: the initial INVITE of each originated leg.
//!
//! A set replaces the relayed lines of its name, `null` removes them, and an
//! add rides only a message that carries none of the name. `100 Trying` and
//! the messages the transaction layer builds on its own (a CANCEL's `200` and
//! the `487` it answers, the ACK of a non-2xx final) carry none, and no scope
//! reaches a media leg. Each route states its own sets: a failover route's
//! replace the first route's from then on, and a decision that applies no
//! route (a reject) states none.
//!
//! A repeat is the message it repeats (RFC 3261 §13.3.1.4, §13.2.2.4; RFC 3262
//! §3), so the un-ACKed 2xx ladders on both faces, the re-ACK of a repeated 2xx
//! and the un-PRACKed reliable provisional ladder repeat the stated lines byte
//! for byte. The originated-leg charging arm (RFC 7315 §5.6) mints nothing
//! where a set states the vector.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{HeaderUpdate, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaScene, B2buaSut, BOB_PORT};
use call::features::{ChargingVectorFeature, StatedHeaders};
use scenario_harness::{Harness, RunReport};
use sip_message::generators::InDialogMethod;
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

/// The second attempt's callee, dialed after bob's refusal reroutes the plan.
const CAROL_PORT: u16 = 5071;

fn lines(v: &[&str]) -> Vec<String> {
    v.iter().map(|l| l.to_string()).collect()
}

fn set(v: &[&str]) -> HeaderUpdate {
    HeaderUpdate::Set(lines(v))
}

fn add(v: &[&str]) -> HeaderUpdate {
    HeaderUpdate::Add(lines(v))
}

/// Sets stating `P-Charging-Vector` as `statement` on every message.
fn charging(statement: HeaderUpdate) -> StatedHeaders {
    let mut s = StatedHeaders::default();
    s.every_message.insert("P-Charging-Vector".into(), statement);
    s
}

/// The scene whose every route states `stated` beside the originated-leg
/// charging arm.
async fn scene(name: &str, stated: StatedHeaders) -> B2buaScene {
    B2buaScene::with_b2bua(name, |bob_port| {
        let engine = ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut route = route_to("127.0.0.1", bob_port);
                route.features.charging_vector = Some(ChargingVectorFeature::default());
                route.features.stated_headers = Some(stated.clone());
                NewCallResponse::Route(route)
            })
            .build();
        B2buaSut::builder(Arc::new(engine))
    })
    .await
}

/// The numbering-plan scene: each call's `X-Api-Call` plan is its decision.
async fn plan_scene(name: &str) -> B2buaScene {
    B2buaScene::with_b2bua(name, |_bob_port| {
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::numbering_plan()))
    })
    .await
}

/// One datagram the SUT sent: its start line, CSeq, status and bytes.
struct Sent {
    start: String,
    cseq: String,
    status: Option<u16>,
    msg: SipMessage,
    raw: Vec<u8>,
}

impl Sent {
    /// The lines of `name` the message carries, in order.
    fn lines(&self, name: &str) -> Vec<String> {
        let name = HeaderName::from(name);
        match &self.msg {
            SipMessage::Request(r) => r.raw(name).map(str::to_string).collect(),
            SipMessage::Response(r) => r.raw(name).map(str::to_string).collect(),
        }
    }

    fn what(&self) -> String {
        format!("`{}` ({})", self.start, self.cseq)
    }

    fn is_trying(&self) -> bool {
        self.status == Some(100)
    }
}

/// Every datagram `from` sent `to`, read with the message parser.
fn sent(report: &RunReport, from: SocketAddr, to: SocketAddr) -> Vec<Sent> {
    report
        .entries()
        .iter()
        .filter(|e| e.from == from && e.to == to)
        .map(|e| {
            let msg = CustomParser::new().parse(&e.raw).expect("the SUT sends parseable SIP");
            let start = String::from_utf8_lossy(&e.raw).lines().next().unwrap_or("").to_string();
            let (cseq, status) = match &msg {
                SipMessage::Request(r) => {
                    (format!("{} {}", r.cseq().seq(), r.cseq().method().as_str()), None)
                }
                SipMessage::Response(r) => {
                    (format!("{} {}", r.cseq().seq(), r.cseq().method().as_str()), Some(r.status()))
                }
            };
            Sent { start, cseq, status, msg, raw: e.raw.clone() }
        })
        .collect()
}

/// Every message but `100 Trying` carries exactly `expected` lines of `name`;
/// the `100`s carry none.
#[track_caller]
fn assert_every_message_carries(who: &str, messages: &[Sent], name: &str, expected: &[&str]) {
    assert!(!messages.is_empty(), "{who}: nothing sent");
    for m in messages {
        let want: Vec<String> = if m.is_trying() { vec![] } else { lines(expected) };
        assert_eq!(m.lines(name), want, "{who}: {} `{name}`", m.what());
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

/// The one message among `messages` whose start line begins `start` with CSeq
/// `cseq`.
#[track_caller]
fn one<'a>(messages: &'a [Sent], start: &str, cseq: &str) -> &'a Sent {
    let found: Vec<&Sent> =
        messages.iter().filter(|m| m.start.starts_with(start) && m.cseq == cseq).collect();
    assert!(!found.is_empty(), "no `{start}` ({cseq}) among {:?}", starts(messages));
    found[0]
}

fn starts(messages: &[Sent]) -> Vec<String> {
    messages.iter().map(Sent::what).collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Every message: a set, its repeats, a removal.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn an_every_message_set_rides_every_message_of_the_call() {
    let s = scene("stated-headers-every-message", charging(set(&[STATED]))).await;

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
    assert_every_message_carries(
        "toward the originator",
        &to_alice,
        "P-Charging-Vector",
        &[STATED],
    );
    assert_every_message_carries(
        "toward the originated leg",
        &to_bob,
        "P-Charging-Vector",
        &[STATED],
    );
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
async fn the_reliable_provisional_ladder_repeats_the_stated_lines() {
    let s = scene("stated-headers-100rel", charging(set(&[STATED]))).await;

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
    s.h.advance(Duration::from_millis(1_600)).await;
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

    let (b2bua, alice, bob) = (s.b2bua.addr, s.alice.addr(), s.bob.addr());
    let report = s.finish().await;
    let to_alice = sent(&report, b2bua, alice);
    assert_every_message_carries(
        "toward the originator",
        &to_alice,
        "P-Charging-Vector",
        &[STATED],
    );
    let to_bob = sent(&report, b2bua, bob);
    assert_every_message_carries(
        "toward the originated leg",
        &to_bob,
        "P-Charging-Vector",
        &[STATED],
    );
    let rungs = copies(&to_alice, "SIP/2.0 183 ", "1 INVITE");
    assert!(rungs.len() >= 3, "two rungs fired: {} copies", rungs.len());
    assert_repeated_whole("the reliable 183", &rungs);
}

/// A stated removal: the originator's vector and the answerer's are taken off
/// every message, and the originated-leg arm mints none.
#[tokio::test(start_paused = true)]
async fn a_stated_removal_takes_every_copy_off_and_mints_none() {
    let s = scene("stated-headers-removed", charging(HeaderUpdate::Remove)).await;

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
    let to_alice = sent(&report, b2bua, alice);
    let to_bob = sent(&report, b2bua, bob);
    assert_every_message_carries("toward the originator", &to_alice, "P-Charging-Vector", &[]);
    assert_every_message_carries("toward the originated leg", &to_bob, "P-Charging-Vector", &[]);
}

/// An add of the vector yields to the originator's own on every message that
/// relays it, and the arm mints none on the originated INVITE: the add is the
/// decision's, and the decision outranks the arm.
#[tokio::test(start_paused = true)]
async fn an_every_message_add_yields_to_a_relayed_line_and_the_arm_mints_none() {
    let s = scene("stated-headers-add", charging(add(&[STATED]))).await;

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
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
    let to_bob = sent(&report, b2bua, bob);
    assert_every_message_carries(
        "toward the originated leg",
        &to_bob,
        "P-Charging-Vector",
        &[STATED],
    );
    let to_alice = sent(&report, b2bua, alice);
    let answer = one(&to_alice, "SIP/2.0 200 ", "1 INVITE");
    assert_eq!(
        answer.lines("P-Charging-Vector"),
        [ANSWERER_OWN],
        "the answer relays the answerer's own: the add yields"
    );
    let bye_ok = one(&to_alice, "SIP/2.0 200 ", "2 BYE");
    assert_eq!(
        bye_ok.lines("P-Charging-Vector"),
        [STATED],
        "a message carrying none takes the add"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Each scope reaches its own messages.
// ─────────────────────────────────────────────────────────────────────────────

/// Every scope at once, over a call that rings, answers, takes a callee
/// re-INVITE and is hung up by the caller.
fn every_scope() -> StatedHeaders {
    let mut s = StatedHeaders::default();
    s.launched_invite.insert("X-Launch".into(), add(&["launch"]));
    s.launched_invite.insert("X-Launch-Kept".into(), add(&["launch"]));
    s.originator_finals.insert("X-Final".into(), set(&["final-1", "final-2"]));
    s.originator_finals.insert("X-Final-Kept".into(), add(&["final"]));
    s.every_message.insert("X-Every".into(), set(&["every"]));
    s.every_message.insert("X-Gone".into(), HeaderUpdate::Remove);
    s.every_message.insert("X-Every-Add".into(), add(&["every"]));
    s
}

#[tokio::test(start_paused = true)]
async fn each_scope_reaches_its_own_messages() {
    let s = scene("stated-headers-scopes", every_scope()).await;

    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("X-Gone", "caller")
        .with_header("X-Every-Add", "caller")
        .with_header("X-Launch-Kept", "caller")
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").with_header("X-Gone", "callee").await;
    call.expect(180).await;
    uas.respond(200, "OK")
        .with_header("X-Gone", "callee")
        .with_header("X-Final-Kept", "callee")
        .with_sdp(ANSWER)
        .await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    s.bob.receive("ACK").await;

    let mut bob_dialog = uas.dialog();
    let mut reinv = bob_dialog.reinvite(Some(BOB_REOFFER)).await;
    s.alice.receive("INVITE").await.respond(200, "OK").with_sdp(ALICE_REANSWER).await;
    reinv.expect(200).await;
    bob_dialog.ack(None).await;
    s.alice.receive("ACK").await;

    let mut bye = alice_dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let (b2bua, alice, bob) = (s.b2bua.addr, s.alice.addr(), s.bob.addr());
    let report = s.finish().await;
    let to_alice = sent(&report, b2bua, alice);
    let to_bob = sent(&report, b2bua, bob);

    // every_message: both legs, every message but the 100.
    assert_every_message_carries("toward the originator", &to_alice, "X-Every", &["every"]);
    assert_every_message_carries("toward the originated leg", &to_bob, "X-Every", &["every"]);
    assert_every_message_carries("toward the originator", &to_alice, "X-Gone", &[]);
    assert_every_message_carries("toward the originated leg", &to_bob, "X-Gone", &[]);
    let invite = one(&to_bob, "INVITE ", "1 INVITE");
    assert_eq!(invite.lines("X-Every-Add"), ["caller"], "the add yields to the relayed line");
    for m in to_bob.iter().filter(|m| m.cseq != "1 INVITE") {
        assert_eq!(m.lines("X-Every-Add"), ["every"], "callee: {} takes the add", m.what());
    }
    for m in to_alice.iter().filter(|m| !m.is_trying()) {
        assert_eq!(m.lines("X-Every-Add"), ["every"], "caller: {} takes the add", m.what());
    }

    // launched_invite: the originated INVITE only.
    assert_eq!(invite.lines("X-Launch"), ["launch"], "the launched INVITE takes the add");
    assert_eq!(invite.lines("X-Launch-Kept"), ["caller"], "and keeps the relayed line");
    for m in to_bob.iter().chain(&to_alice).filter(|m| !std::ptr::eq(*m, invite)) {
        assert!(m.lines("X-Launch").is_empty(), "{} is no launched INVITE", m.what());
    }

    // originator_finals: the caller's 200 to her INVITE, nothing else.
    let answer = one(&to_alice, "SIP/2.0 200 ", "1 INVITE");
    assert_eq!(answer.lines("X-Final"), ["final-1", "final-2"], "the final takes both lines");
    assert_eq!(answer.lines("X-Final-Kept"), ["callee"], "and keeps the callee's relayed line");
    for m in to_bob.iter().chain(&to_alice).filter(|m| !std::ptr::eq(*m, answer)) {
        assert!(m.lines("X-Final").is_empty(), "{} is no final to the initial INVITE", m.what());
    }
}

/// The caller cancels a ringing call: the CANCEL toward the callee carries the
/// every-message set; what the transaction layer builds on its own — the `487`
/// answering her INVITE, the `200` to her CANCEL, the ACK of the callee's
/// `487` — carries none.
#[tokio::test(start_paused = true)]
async fn a_cancel_carries_the_every_message_set_and_the_layers_own_messages_none() {
    let s = scene("stated-headers-cancel", every_scope()).await;

    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    s.bob.receive("CANCEL").await.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    s.bob.receive("ACK").await;

    let (b2bua, alice, bob) = (s.b2bua.addr, s.alice.addr(), s.bob.addr());
    let report = s.finish().await;
    let to_alice = sent(&report, b2bua, alice);
    let to_bob = sent(&report, b2bua, bob);

    assert_eq!(one(&to_bob, "INVITE ", "1 INVITE").lines("X-Every"), ["every"]);
    assert_eq!(one(&to_bob, "CANCEL ", "1 CANCEL").lines("X-Every"), ["every"]);
    assert_eq!(
        one(&to_bob, "ACK ", "1 ACK").lines("X-Every"),
        Vec::<String>::new(),
        "the ACK of a non-2xx is the transaction layer's"
    );
    assert_eq!(one(&to_alice, "SIP/2.0 180 ", "1 INVITE").lines("X-Every"), ["every"]);
    let terminated = one(&to_alice, "SIP/2.0 487 ", "1 INVITE");
    for name in ["X-Every", "X-Final", "X-Every-Add"] {
        assert!(terminated.lines(name).is_empty(), "the layer's 487 carries no `{name}`");
    }
    let cancel_ok = one(&to_alice, "SIP/2.0 200 ", "1 CANCEL");
    assert!(
        cancel_ok.lines("X-Every").is_empty() && cancel_ok.lines("X-Final").is_empty(),
        "the 200 to the CANCEL is the transaction layer's own"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Each decision states its own sets.
// ─────────────────────────────────────────────────────────────────────────────

/// A route's sets: `X-Every` on every message, `X-Final` on the finals.
fn route_sets(tag: &str) -> serde_json::Value {
    serde_json::json!({
        "launched_invite": {},
        "originator_finals": {"X-Final": [tag]},
        "every_message": {"X-Every": [tag]},
    })
}

/// bob refuses with a 486 after ringing; the plan reroutes to carol, whose
/// route states other sets: carol's leg and everything the caller receives
/// after the reroute carry the second route's, the 180 before it the first's.
#[tokio::test(start_paused = true)]
async fn a_failover_route_states_its_own_sets() {
    let s = plan_scene("stated-headers-failover-route").await;
    let carol = s.h.agent("carol", &format!("127.0.0.1:{CAROL_PORT}")).await;
    let plan = serde_json::json!({
        "routes": [
            {"destination": {"host": "127.0.0.1", "port": BOB_PORT},
             "stated_headers": route_sets("first")},
            {"destination": {"host": "127.0.0.1", "port": CAROL_PORT},
             "stated_headers": route_sets("second")},
        ],
    })
    .to_string();

    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut bob_uas = s.bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(486, "Busy Here").await;
    s.bob.receive("ACK").await;

    let mut carol_uas = carol.receive("INVITE").await;
    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;
    let mut bye = dialog.bye().await;
    carol.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let (b2bua, alice, bob, carol_addr) =
        (s.b2bua.addr, s.alice.addr(), s.bob.addr(), carol.addr());
    let report = s.finish().await;
    let to_bob = sent(&report, b2bua, bob);
    let to_carol = sent(&report, b2bua, carol_addr);
    let to_alice = sent(&report, b2bua, alice);

    assert_eq!(one(&to_bob, "INVITE ", "1 INVITE").lines("X-Every"), ["first"]);
    assert_every_message_carries("toward the rerouted leg", &to_carol, "X-Every", &["second"]);
    assert_eq!(one(&to_alice, "SIP/2.0 180 ", "1 INVITE").lines("X-Every"), ["first"]);
    let answer = one(&to_alice, "SIP/2.0 200 ", "1 INVITE");
    assert_eq!(answer.lines("X-Every"), ["second"], "the reroute's set from then on");
    assert_eq!(answer.lines("X-Final"), ["second"], "the reroute's finals set");
    assert_eq!(one(&to_alice, "SIP/2.0 200 ", "2 BYE").lines("X-Every"), ["second"]);
}

/// bob refuses; the plan rejects with its own final: the reject applies no
/// route, so it states no sets and the first route's leave the call — the
/// caller's final carries the reject's own header and none of the route's.
#[tokio::test(start_paused = true)]
async fn a_failover_reject_ends_the_routes_sets() {
    let s = plan_scene("stated-headers-failover-reject").await;
    let plan = serde_json::json!({
        "routes": [{"destination": {"host": "127.0.0.1", "port": BOB_PORT},
                    "stated_headers": route_sets("first")}],
        "on_exhausted": {"action": "reject", "code": 480, "reason": "Temporarily Unavailable",
                         "update_headers": {"Reason": "Q.850;cause=34"}},
    })
    .to_string();

    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut bob_uas = s.bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(486, "Busy Here").await;
    s.bob.receive("ACK").await;
    call.expect(480).await;

    let (b2bua, alice) = (s.b2bua.addr, s.alice.addr());
    let report = s.finish().await;
    let to_alice = sent(&report, b2bua, alice);
    assert_eq!(one(&to_alice, "SIP/2.0 180 ", "1 INVITE").lines("X-Every"), ["first"]);
    let rejected = one(&to_alice, "SIP/2.0 480 ", "1 INVITE");
    assert_eq!(rejected.lines("Reason"), ["Q.850;cause=34"], "the reject's own statement");
    assert!(rejected.lines("X-Every").is_empty(), "the route's every-message set ended");
    assert!(rejected.lines("X-Final").is_empty(), "the route's finals set ended");
}

/// bob refuses and the plan declines to fail over: the relayed failure is the
/// route's final, so it carries the route's sets.
#[tokio::test(start_paused = true)]
async fn a_relayed_failure_carries_the_routes_sets() {
    let s = plan_scene("stated-headers-relayed-failure").await;
    let plan = serde_json::json!({
        "routes": [{"destination": {"host": "127.0.0.1", "port": BOB_PORT},
                    "stated_headers": route_sets("first")}],
    })
    .to_string();

    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut bob_uas = s.bob.receive("INVITE").await;
    bob_uas.respond(486, "Busy Here").await;
    s.bob.receive("ACK").await;
    call.expect(486).await;

    let (b2bua, alice) = (s.b2bua.addr, s.alice.addr());
    let report = s.finish().await;
    let to_alice = sent(&report, b2bua, alice);
    let failed = one(&to_alice, "SIP/2.0 486 ", "1 INVITE");
    assert_eq!(failed.lines("X-Every"), ["first"]);
    assert_eq!(failed.lines("X-Final"), ["first"]);
}

// ─────────────────────────────────────────────────────────────────────────────
// A media leg takes none.
// ─────────────────────────────────────────────────────────────────────────────

const MRF_SDP: &str = "v=0\r\no=mrf 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const MRF_PORT: u16 = 5672;
const DEST_PORT: u16 = 5952;

/// The announcement service parks a media leg toward the MRF before it dials
/// the destination: nothing the B2BUA sends the MRF carries the route's sets;
/// the caller and the destination take the every-message set, the launched
/// INVITE toward the destination its add.
#[tokio::test(start_paused = true)]
async fn a_media_leg_takes_no_stated_header() {
    let h = Harness::new("stated-headers-media-leg");
    let alice = h.agent("alice", "127.0.0.1:5906").await;
    let mrf = h.agent("mrf", &format!("127.0.0.1:{MRF_PORT}")).await;
    let dest = h.agent("dest", &format!("127.0.0.1:{DEST_PORT}")).await;
    let engine = ScriptedDecisionEngine::builder()
        .fallback(|_req| {
            let mut r = route_to("127.0.0.1", DEST_PORT);
            r.features.stated_headers = Some(every_scope());
            r.service_ext.insert(
                "announcement".into(),
                serde_json::json!({
                    "clip_id": "intro-001",
                    "mrf_host": "127.0.0.1",
                    "mrf_port": MRF_PORT,
                    "dest_host": "127.0.0.1",
                    "dest_port": DEST_PORT,
                    "defer_routing": true,
                }),
            );
            NewCallResponse::Route(r)
        })
        .build();
    let b2bua = B2buaSut::builder(Arc::new(engine))
        .services(vec![announcement::service()])
        .start(&h, "b2bua", "127.0.0.1:5926")
        .await;

    let mut call = alice.invite(&dest).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut mrf_uas = mrf.receive("INVITE").await;
    mrf_uas.respond(200, "OK").with_sdp(MRF_SDP).await;
    mrf.receive("ACK").await;
    let mut mrf_dialog = mrf_uas.dialog();
    call.expect(183).await;
    mrf.receive("INFO").await.respond(200, "OK").await;
    let done_body = String::from_utf8(announcement::mscml::build_response(200)).unwrap();
    let mut done = mrf_dialog
        .send_request(InDialogMethod::Info)
        .with_header("Content-Type", "application/mediaservercontrol+xml")
        .with_sdp(&done_body)
        .send()
        .await;
    done.expect(200).await;
    mrf.receive("BYE").await.respond(200, "OK").await;
    let mut dest_uas = dest.receive("INVITE").await;
    dest_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    dest.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    dest.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let (b2bua_addr, alice_addr, mrf_addr, dest_addr) =
        (b2bua.addr, alice.addr(), mrf.addr(), dest.addr());
    let report = h.finish().await;

    let to_mrf = sent(&report, b2bua_addr, mrf_addr);
    assert!(!to_mrf.is_empty(), "the media leg was used");
    for m in &to_mrf {
        for name in ["X-Every", "X-Every-Add", "X-Launch", "X-Final"] {
            assert!(m.lines(name).is_empty(), "media leg: {} takes no `{name}`", m.what());
        }
    }
    let to_dest = sent(&report, b2bua_addr, dest_addr);
    assert_every_message_carries("toward the destination", &to_dest, "X-Every", &["every"]);
    assert_eq!(one(&to_dest, "INVITE ", "1 INVITE").lines("X-Launch"), ["launch"]);
    let to_alice = sent(&report, b2bua_addr, alice_addr);
    assert_every_message_carries("toward the originator", &to_alice, "X-Every", &["every"]);
}

/// A BYE under a To-tag naming no dialog is refused `481` on the call's
/// behalf (RFC 3261 §12.2.2): the refusal is a message of the caller's leg and
/// takes the every-message set.
#[tokio::test(start_paused = true)]
async fn an_in_call_refusal_takes_the_every_message_set() {
    let s = scene("stated-headers-in-call-481", every_scope()).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;

    // The scripted caller is the deviant here: it names a dialog nobody holds.
    s.h.allow_violation(
        "mid-dialog-tags",
        "the BYE under a foreign To-tag is the deviation under test (RFC 3261 §12.2.2)",
    );
    let cseq_before = dialog.local_cseq();
    let mut foreign =
        dialog.send_request(InDialogMethod::Bye).with_to_tag("no-dialog-here").send().await;
    dialog.set_local_cseq(cseq_before);
    let refused = foreign.expect(481).await;
    let lines = |name: &str| -> Vec<String> {
        refused.raw(HeaderName::from(name)).map(str::to_string).collect()
    };
    assert_eq!(lines("X-Every"), ["every"], "the refusal takes the every-message set");
    assert!(lines("X-Final").is_empty(), "a refused BYE is no final to the initial INVITE");

    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let _ = s.finish().await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Two scopes naming one header: the wider applies first, the narrower against
// its result.
// ─────────────────────────────────────────────────────────────────────────────

fn combined() -> StatedHeaders {
    let mut s = StatedHeaders::default();
    // (a) a launched-INVITE replacement over an every-message one.
    s.every_message.insert("X-A".into(), set(&["every"]));
    s.launched_invite.insert("X-A".into(), set(&["launched"]));
    // (b) an every-message removal under a finals add.
    s.every_message.insert("X-B".into(), HeaderUpdate::Remove);
    s.originator_finals.insert("X-B".into(), add(&["final"]));
    // (c) an every-message removal under a launched-INVITE add.
    s.every_message.insert("X-C".into(), HeaderUpdate::Remove);
    s.launched_invite.insert("x-c".into(), add(&["launched"]));
    s
}

#[tokio::test(start_paused = true)]
async fn the_wider_scope_applies_first_and_the_narrower_against_its_result() {
    let s = scene("stated-headers-combined", combined()).await;
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("X-B", "caller")
        .with_header("X-C", "caller")
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_header("X-B", "callee").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let (b2bua, alice, bob) = (s.b2bua.addr, s.alice.addr(), s.bob.addr());
    let report = s.finish().await;
    let to_alice = sent(&report, b2bua, alice);
    let to_bob = sent(&report, b2bua, bob);
    let invite = one(&to_bob, "INVITE ", "1 INVITE");
    assert_eq!(invite.lines("X-A"), ["launched"], "(a) the narrower replacement stands");
    assert!(invite.lines("X-B").is_empty(), "the removal takes the caller's line off");
    assert_eq!(invite.lines("X-C"), ["launched"], "(c) the add lands once the removal ran");
    let answer = one(&to_alice, "SIP/2.0 200 ", "1 INVITE");
    assert_eq!(answer.lines("X-B"), ["final"], "(b) the add lands once the removal ran");
    assert_eq!(answer.lines("X-A"), ["every"]);
    assert!(one(&to_bob, "BYE ", "2 BYE").lines("X-C").is_empty(), "(c) on the INVITE only");
}

// ─────────────────────────────────────────────────────────────────────────────
// Headers bound to the transaction, a negotiation or the body are never
// restated; capability advertisements only at the mint.
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn bound_headers_and_advertisements_are_never_stamped() {
    let mut stated = StatedHeaders::default();
    for (name, value) in [
        ("Require", "zz-require"),
        ("Session-Expires", "4242"),
        ("Event", "zz-event"),
        ("Content-Disposition", "zz-disposition"),
        ("Supported", "zz-supported"),
        ("Allow", "zz-allow"),
    ] {
        stated.every_message.insert(name.into(), set(&[value]));
        stated.launched_invite.insert(name.into(), add(&[value]));
        stated.originator_finals.insert(name.into(), set(&[value]));
    }
    let s = scene("stated-headers-refused", stated).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let (b2bua, alice, bob) = (s.b2bua.addr, s.alice.addr(), s.bob.addr());
    let report = s.finish().await;
    for m in sent(&report, b2bua, alice).iter().chain(&sent(&report, b2bua, bob)) {
        let raw = String::from_utf8_lossy(&m.raw);
        assert!(
            !raw.contains("zz-") && !raw.contains("4242"),
            "{} carries no restated bound header: {raw}",
            m.what()
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// A reject's own sets: its adds yield to a line its final already carries.
// ─────────────────────────────────────────────────────────────────────────────

/// bob refuses with a `Reason` of his own; the plan rejects stating two adds:
/// the callee's `Reason` rides the decision-authored final, the other add lands.
#[tokio::test(start_paused = true)]
async fn a_failover_rejects_adds_yield_to_the_lines_its_final_carries() {
    let s = plan_scene("stated-headers-failover-reject-adds").await;
    let plan = serde_json::json!({
        "routes": [{"destination": {"host": "127.0.0.1", "port": BOB_PORT},
                    "stated_headers": route_sets("first")}],
        "on_exhausted": {"action": "reject", "code": 480, "reason": "Temporarily Unavailable",
                         "update_headers": {
                             "Reason": {"add": ["Q.850;cause=34"]},
                             "X-Reject": {"add": ["r"]}}},
    })
    .to_string();
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .through(s.b2bua.addr)
        .send()
        .await;
    s.bob
        .receive("INVITE")
        .await
        .respond(486, "Busy Here")
        .with_header("Reason", "Q.850;cause=17")
        .await;
    s.bob.receive("ACK").await;
    let rejected = call.expect(480).await;
    let lines = |name: &str| -> Vec<String> {
        rejected.raw(HeaderName::from(name)).map(str::to_string).collect()
    };
    assert_eq!(lines("Reason"), ["Q.850;cause=17"], "the add yields to the relayed Reason");
    assert_eq!(lines("X-Reject"), ["r"], "an add of a name the final lacks lands");
    assert!(lines("X-Every").is_empty(), "the route's set ended");
    let _ = s.finish().await;
}

/// An initial reject's adds land on the final it authors.
#[tokio::test(start_paused = true)]
async fn an_initial_rejects_adds_land_on_its_final() {
    let s = plan_scene("stated-headers-initial-reject-adds").await;
    let plan = serde_json::json!({
        "action": "reject", "code": 403, "reason": "Forbidden",
        "update_headers": {"X-Reject": {"add": ["r"]}},
    })
    .to_string();
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .through(s.b2bua.addr)
        .send()
        .await;
    let rejected = call.expect(403).await;
    assert_eq!(
        rejected.raw(HeaderName::from("X-Reject")).map(str::to_string).collect::<Vec<_>>(),
        ["r"]
    );
    let _ = s.finish().await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Answers the router authors on the call's behalf.
// ─────────────────────────────────────────────────────────────────────────────

/// A store fault refuses an in-dialog BYE `500` on the resident call's behalf:
/// the refusal takes the call's every-message set.
#[tokio::test(start_paused = true)]
async fn a_store_fault_refusal_takes_the_resident_calls_set() {
    use b2bua::store::{StoreFaultPoint, StoreFaults};
    let faults = StoreFaults::default();
    let route_faults = faults.clone();
    let s = B2buaScene::with_b2bua("stated-headers-store-fault", move |bob_port| {
        let engine = ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut route = route_to("127.0.0.1", bob_port);
                route.features.stated_headers = Some(every_scope());
                NewCallResponse::Route(route)
            })
            .build();
        B2buaSut::builder(Arc::new(engine)).with_store_faults(route_faults)
    })
    .await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    s.bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    s.h.advance(Duration::from_millis(500)).await;

    faults.arm(StoreFaultPoint::LiveInDialog);
    let mut bye1 = dialog.bye().await;
    let refused = bye1.expect(500).await;
    assert_eq!(
        refused.raw(HeaderName::from("X-Every")).map(str::to_string).collect::<Vec<_>>(),
        ["every"],
        "the store-fault refusal takes the every-message set"
    );
    faults.disarm(StoreFaultPoint::LiveInDialog);
    let mut bye2 = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye2.expect(200).await;
    let _ = s.finish().await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Every in-dialog method, both ways, and the stack's own requests.
// ─────────────────────────────────────────────────────────────────────────────

/// UPDATE and INFO both ways, a relayed OPTIONS, and the keepalive OPTIONS the
/// stack originates on both legs: every one and every answer carries the
/// every-message set.
#[tokio::test(start_paused = true)]
async fn every_in_dialog_method_carries_the_every_message_set() {
    let s = scene("stated-headers-methods", every_scope()).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    let mut update = alice_dialog.request(InDialogMethod::Update, None).await;
    s.bob.receive("UPDATE").await.respond(200, "OK").await;
    update.expect(200).await;
    let mut update = bob_dialog.request(InDialogMethod::Update, None).await;
    s.alice.receive("UPDATE").await.respond(200, "OK").await;
    update.expect(200).await;
    let mut info = alice_dialog
        .send_request(InDialogMethod::Info)
        .with_body("application/x-info", b"a".to_vec())
        .send()
        .await;
    s.bob.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
    let mut info = bob_dialog
        .send_request(InDialogMethod::Info)
        .with_body("application/x-info", b"b".to_vec())
        .send()
        .await;
    s.alice.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
    let mut options = alice_dialog.send_request(InDialogMethod::Options).send().await;
    s.bob.receive("OPTIONS").await.respond(200, "OK").await;
    options.expect(200).await;

    // The keepalive the stack originates on both legs (default 30 s).
    s.h.advance(Duration::from_secs(30)).await;
    s.alice.receive("OPTIONS").await.respond(200, "OK").await;
    s.bob.receive("OPTIONS").await.respond(200, "OK").await;

    let mut bye = alice_dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let (b2bua, alice, bob) = (s.b2bua.addr, s.alice.addr(), s.bob.addr());
    let report = s.finish().await;
    let to_alice = sent(&report, b2bua, alice);
    let to_bob = sent(&report, b2bua, bob);
    assert_every_message_carries("toward the originator", &to_alice, "X-Every", &["every"]);
    assert_every_message_carries("toward the originated leg", &to_bob, "X-Every", &["every"]);
    for (who, msgs) in [("caller", &to_alice), ("callee", &to_bob)] {
        for (start, method) in [
            ("UPDATE ", " UPDATE"),
            ("SIP/2.0 200 ", " UPDATE"),
            ("INFO ", " INFO"),
            ("SIP/2.0 200 ", " INFO"),
            ("OPTIONS ", " OPTIONS"),
        ] {
            assert!(
                msgs.iter().any(|m| m.start.starts_with(start) && m.cseq.ends_with(method)),
                "{who}: a `{start}` ({method}) went out: {:?}",
                starts(msgs)
            );
        }
    }
    assert!(
        to_alice
            .iter()
            .any(|m| m.start.starts_with("SIP/2.0 200 ") && m.cseq.ends_with(" OPTIONS")),
        "the relayed OPTIONS's answer reached the caller"
    );
}

/// The PRACK the stack originates toward a reliable callee under fake-PRACK
/// carries the every-message set.
#[tokio::test(start_paused = true)]
async fn the_stacks_own_prack_carries_the_every_message_set() {
    let s = B2buaScene::with_b2bua("stated-headers-fake-prack", |bob_port| {
        let engine = ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut route = b2bua::decision::test_adapter::route_to_with_18x(
                    "127.0.0.1",
                    bob_port,
                    call::features::RelayFirst18xStrategy::FakePrack,
                );
                route.features.stated_headers = Some(every_scope());
                NewCallResponse::Route(route)
            })
            .build();
        B2buaSut::builder(Arc::new(engine))
    })
    .await;
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
        .with_header("RSeq", "1")
        .with_sdp(ANSWER)
        .await;
    call.expect(180).await;
    let mut prack = s.bob.receive("PRACK").await;
    assert_eq!(
        prack.request().raw(HeaderName::from("X-Every")).map(str::to_string).collect::<Vec<_>>(),
        ["every"],
        "the stack's own PRACK"
    );
    prack.respond(200, "OK").await;
    uas.respond(200, "OK").await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let _ = s.finish().await;
}

/// A deployment that leaves media legs uncharged: the originated-leg arm mints
/// no vector on one and the caller's own is not relayed to it, while the
/// destination leg relays the caller's.
#[tokio::test(start_paused = true)]
async fn a_media_leg_takes_no_charging_vector_where_the_deployment_says_so() {
    media_leg_charging(true).await;
}

/// By default a media leg is charged like any originated leg: the caller's
/// vector relays to it.
#[tokio::test(start_paused = true)]
async fn a_media_leg_relays_the_callers_vector_by_default() {
    media_leg_charging(false).await;
}

async fn media_leg_charging(uncharged: bool) {
    let h = Harness::new(format!("stated-headers-media-leg-charging-{uncharged}"));
    let alice = h.agent("alice", "127.0.0.1:5907").await;
    let mrf = h.agent("mrf", &format!("127.0.0.1:{MRF_PORT}")).await;
    let dest = h.agent("dest", &format!("127.0.0.1:{DEST_PORT}")).await;
    let engine = ScriptedDecisionEngine::builder()
        .fallback(move |_req| {
            let mut r = route_to("127.0.0.1", DEST_PORT);
            r.features.charging_vector = Some(ChargingVectorFeature::default());
            r.features.uncharged_media_legs = uncharged;
            r.service_ext.insert(
                "announcement".into(),
                serde_json::json!({
                    "clip_id": "intro-001",
                    "mrf_host": "127.0.0.1",
                    "mrf_port": MRF_PORT,
                    "dest_host": "127.0.0.1",
                    "dest_port": DEST_PORT,
                    "defer_routing": true,
                }),
            );
            NewCallResponse::Route(r)
        })
        .build();
    let b2bua = B2buaSut::builder(Arc::new(engine))
        .services(vec![announcement::service()])
        .start(&h, "b2bua", "127.0.0.1:5927")
        .await;

    let mut call = alice
        .invite(&dest)
        .with_sdp(OFFER)
        .with_header("P-Charging-Vector", ORIGINATOR_OWN)
        .through(b2bua.addr)
        .send()
        .await;
    let mut mrf_uas = mrf.receive("INVITE").await;
    let toward_mrf: Vec<String> =
        mrf_uas.request().raw(HeaderName::from("P-Charging-Vector")).map(str::to_string).collect();
    if uncharged {
        assert!(toward_mrf.is_empty(), "no vector toward the media resource, minted or relayed");
    } else {
        assert_eq!(toward_mrf, [ORIGINATOR_OWN], "the caller's vector relays as to any leg");
    }
    mrf_uas.respond(200, "OK").with_sdp(MRF_SDP).await;
    mrf.receive("ACK").await;
    let mut mrf_dialog = mrf_uas.dialog();
    call.expect(183).await;
    mrf.receive("INFO").await.respond(200, "OK").await;
    let done_body = String::from_utf8(announcement::mscml::build_response(200)).unwrap();
    let mut done = mrf_dialog
        .send_request(InDialogMethod::Info)
        .with_header("Content-Type", "application/mediaservercontrol+xml")
        .with_sdp(&done_body)
        .send()
        .await;
    done.expect(200).await;
    mrf.receive("BYE").await.respond(200, "OK").await;
    let mut dest_uas = dest.receive("INVITE").await;
    assert_eq!(
        dest_uas.request().raw(HeaderName::from("P-Charging-Vector")).collect::<Vec<_>>(),
        [ORIGINATOR_OWN],
        "the destination relays the caller's vector"
    );
    dest_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    dest.receive("ACK").await;
    let mut bye = alice_dialog.bye().await;
    dest.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let (b2bua_addr, mrf_addr) = (b2bua.addr, mrf.addr());
    let report = h.finish().await;
    if uncharged {
        for m in sent(&report, b2bua_addr, mrf_addr) {
            assert!(
                m.lines("P-Charging-Vector").is_empty(),
                "media leg: {} carries none",
                m.what()
            );
        }
    }
}

/// A CANCEL of an INVITE answered long ago reaches the call with no
/// transaction to match it: the `481` the router answers on the call's behalf
/// (RFC 3261 §9.2) takes the caller leg's every-message set.
#[tokio::test(start_paused = true)]
async fn a_stray_cancel_refusal_takes_the_calls_set() {
    let s = scene("stated-headers-stray-cancel", every_scope()).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;

    // Past Timer L, the INVITE server transaction is gone; the keepalive at
    // 30 s is answered on the way.
    s.h.advance(Duration::from_secs(30)).await;
    s.alice.receive("OPTIONS").await.respond(200, "OK").await;
    s.bob.receive("OPTIONS").await.respond(200, "OK").await;
    s.h.advance(Duration::from_secs(10)).await;

    s.h.waive(
        scenario_harness::WaiverScope::rule(
            "no-cancel-after-final",
            "the CANCEL of an answered INVITE is the deviation under test (RFC 3261 §9.1)",
        )
        .on_party("alice"),
    );
    let mut cxl = call.cancel().await;
    let refused = cxl.expect(481).await;
    assert_eq!(
        refused.raw(HeaderName::from("X-Every")).map(str::to_string).collect::<Vec<_>>(),
        ["every"],
        "the stray CANCEL's 481 takes the every-message set"
    );

    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let _ = s.finish().await;
}

/// A route's `update_headers` adds yield to the INVITE as minted, on the
/// first route's leg and on the leg a failover route mints alike.
#[tokio::test(start_paused = true)]
async fn a_routes_adds_yield_to_the_invite_as_minted() {
    let s = plan_scene("stated-headers-mint-adds").await;
    let carol = s.h.agent("carol", &format!("127.0.0.1:{CAROL_PORT}")).await;
    let adds = serde_json::json!({"X-Add": {"add": ["route"]}, "X-Kept": {"add": ["route"]}});
    let plan = serde_json::json!({
        "routes": [
            {"destination": {"host": "127.0.0.1", "port": BOB_PORT}, "update_headers": adds},
            {"destination": {"host": "127.0.0.1", "port": CAROL_PORT}, "update_headers": adds},
        ],
    })
    .to_string();
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .with_header("X-Kept", "caller")
        .through(s.b2bua.addr)
        .send()
        .await;
    let mut bob_uas = s.bob.receive("INVITE").await;
    assert_minted_adds("first", bob_uas.request());
    bob_uas.respond(486, "Busy Here").await;
    s.bob.receive("ACK").await;
    let mut carol_uas = carol.receive("INVITE").await;
    assert_minted_adds("failover", carol_uas.request());
    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;
    let mut bye = dialog.bye().await;
    carol.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let _ = s.finish().await;
}

#[track_caller]
fn assert_minted_adds(who: &str, req: &sip_message::SipRequest) {
    assert_eq!(req.raw(HeaderName::from("X-Add")).collect::<Vec<_>>(), ["route"], "{who}");
    assert_eq!(req.raw(HeaderName::from("X-Kept")).collect::<Vec<_>>(), ["caller"], "{who}");
}

// ─────────────────────────────────────────────────────────────────────────────
// A leg's own set; the layer's 487 to a cancelled re-INVITE; refusals.
// ─────────────────────────────────────────────────────────────────────────────

/// A leg the call states a set of its own for takes that set instead of the
/// call's, on every message of the leg; the other legs keep the call's.
#[tokio::test(start_paused = true)]
async fn a_leg_with_its_own_set_takes_it_instead_of_the_calls() {
    let s = plan_scene("stated-headers-leg-set").await;
    let plan = serde_json::json!({
        "destination": {"host": "127.0.0.1", "port": BOB_PORT},
        "stated_headers": {
            "every_message": {"X-Every": ["call"]},
            "legs": {"b-1": {"every_message": {"X-Leg": ["leg"]},
                             "launched_invite": {"X-Leg-Launch": {"add": ["leg"]}}}},
        },
    })
    .to_string();
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .through(s.b2bua.addr)
        .send()
        .await;
    s.bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let (b2bua, alice, bob) = (s.b2bua.addr, s.alice.addr(), s.bob.addr());
    let report = s.finish().await;
    let to_bob = sent(&report, b2bua, bob);
    let to_alice = sent(&report, b2bua, alice);
    assert_every_message_carries("the leg with its own set", &to_bob, "X-Leg", &["leg"]);
    assert_every_message_carries("the leg with its own set", &to_bob, "X-Every", &[]);
    assert_eq!(one(&to_bob, "INVITE ", "1 INVITE").lines("X-Leg-Launch"), ["leg"]);
    assert_every_message_carries("the caller's leg", &to_alice, "X-Every", &["call"]);
    assert_every_message_carries("the caller's leg", &to_alice, "X-Leg", &[]);
}

/// The caller cancels her pending re-INVITE: the `487` the transaction layer
/// answers it with is its own and carries no set.
#[tokio::test(start_paused = true)]
async fn the_layers_487_to_a_cancelled_reinvite_carries_no_set() {
    let s = scene("stated-headers-reinvite-cancel", every_scope()).await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let mut uas = s.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;

    let mut reinv = dialog.reinvite(Some(ALICE_REANSWER)).await;
    let mut bob_reinv = s.bob.receive("INVITE").await;
    let mut cxl = reinv.cancel().await;
    cxl.expect(200).await;
    let terminated = reinv.expect(487).await;
    bob_reinv.respond(100, "Trying").await;
    s.bob.receive("CANCEL").await.respond(200, "OK").await;
    bob_reinv.respond(487, "Request Terminated").await;
    s.bob.receive("ACK").await;
    for name in ["X-Every", "X-Final"] {
        assert!(
            terminated.raw(HeaderName::from(name)).next().is_none(),
            "the layer's 487 to the re-INVITE carries no `{name}`"
        );
    }
    let mut bye = dialog.bye().await;
    s.bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let _ = s.finish().await;
}

/// A failover redirect the stack refuses — not a 3xx, or a target that does
/// not read — speaks only for itself: the caller's 500 carries the route's
/// set and none of the refused redirect's adds.
#[tokio::test(start_paused = true)]
async fn a_refused_failover_redirect_carries_the_routes_set_and_nothing_of_its_own() {
    for (name, redirect) in [
        (
            "not-a-3xx",
            serde_json::json!({"action": "redirect", "code": 404,
                "contacts": [{"uri": "sip:target@192.0.2.9"}],
                "update_headers": {"X-Redirect": {"add": ["r"]}}}),
        ),
        (
            "unreadable-target",
            serde_json::json!({"action": "redirect", "code": 302,
                "contacts": [{"uri": "not a uri at all"}],
                "update_headers": {"X-Redirect": {"add": ["r"]}}}),
        ),
    ] {
        let s = plan_scene(&format!("stated-headers-refused-redirect-{name}")).await;
        let plan = serde_json::json!({
            "routes": [{"destination": {"host": "127.0.0.1", "port": BOB_PORT},
                        "stated_headers": route_sets("first")}],
            "on_exhausted": redirect,
        })
        .to_string();
        let mut call = s
            .alice
            .invite(&s.bob)
            .with_sdp(OFFER)
            .with_header("X-Api-Call", &plan)
            .through(s.b2bua.addr)
            .send()
            .await;
        s.bob.receive("INVITE").await.respond(486, "Busy Here").await;
        s.bob.receive("ACK").await;
        let refused = call.expect(500).await;
        let lines = |n: &str| -> Vec<String> {
            refused.raw(HeaderName::from(n)).map(str::to_string).collect()
        };
        assert_eq!(lines("X-Every"), ["first"], "{name}: the route's set stays");
        assert_eq!(lines("X-Final"), ["first"], "{name}: the route's finals set");
        assert!(lines("X-Redirect").is_empty(), "{name}: nothing of the refused redirect");
        let _ = s.finish().await;
    }
}

/// A tentative set and the one it replaced: the callee's re-INVITE goes
/// unanswered and its timeout ends the call. Every message before the
/// termination carries the tentative set; the termination takes the earlier
/// one back, so the BYEs and the caller's 487 carry it.
#[tokio::test(start_paused = true)]
async fn a_termination_takes_back_the_set_a_tentative_one_replaced() {
    let s = B2buaScene::with_b2bua("stated-headers-tentative", |_bob_port| {
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::numbering_plan()))
            .tune(|c| c.keepalive_interval_sec = 3_600)
    })
    .await;
    let plan = serde_json::json!({
        "destination": {"host": "127.0.0.1", "port": BOB_PORT},
        "stated_headers": {
            "every_message": {"X-Set": ["tentative"]},
            "reverts_to": {"every_message": {"X-Set": ["earlier"]}},
        },
    })
    .to_string();
    let mut call = s
        .alice
        .invite(&s.bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .through(s.b2bua.addr)
        .send()
        .await;
    s.bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    s.bob.receive("ACK").await;
    let mut reinvite = dialog.reinvite(Some(ALICE_REANSWER)).await;
    let _unanswered = s.bob.receive("INVITE").await;
    s.h.advance(Duration::from_secs(40)).await;
    reinvite.expect(487).await;
    s.bob.receive_absorbing("BYE", &["INVITE"]).await.respond(200, "OK").await;
    s.alice.receive_absorbing("BYE", &["ACK"]).await.respond(200, "OK").await;

    let (b2bua, alice, bob) = (s.b2bua.addr, s.alice.addr(), s.bob.addr());
    let report = s.finish().await;
    let to_bob = sent(&report, b2bua, bob);
    let to_alice = sent(&report, b2bua, alice);
    assert_eq!(one(&to_bob, "INVITE ", "1 INVITE").lines("X-Set"), ["tentative"]);
    assert_eq!(one(&to_alice, "SIP/2.0 200", "1 INVITE").lines("X-Set"), ["tentative"]);
    let bye = |messages: &[Sent]| {
        messages.iter().find(|m| m.start.starts_with("BYE ")).expect("a BYE").lines("X-Set")
    };
    assert_eq!(bye(&to_bob), ["earlier"]);
    assert_eq!(bye(&to_alice), ["earlier"]);
    assert_eq!(one(&to_alice, "SIP/2.0 487", "2 INVITE").lines("X-Set"), ["earlier"]);
}
