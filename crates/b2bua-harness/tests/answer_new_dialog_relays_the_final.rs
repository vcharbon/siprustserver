//! RFC 3261 §16.6 on the a-side fork-confirm: the `200` that
//! `AnswerALegNewDialog` mints under a fresh To-tag A2 delivers a callee final,
//! so it carries that final's relayable lines exactly as the plain relay
//! would — `Privacy` above all, the callee's own identity line, and a vendor
//! header this stack models nothing about.
//!
//! The shape is the MRF ring-back fork: the caller is in an early dialog A1
//! from the callee's `183`, then a service rule supersedes the core
//! `confirm-dialog` and answers her under A2 with the callee's `200` as the
//! delivered final. RFC 3261 §13.3.1.4 is put to the bar too: a rung repeats
//! the answer byte for byte, so the relayed lines survive the repeat.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::{Harness, RunReport};
use sip_message::header::HeaderName;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const EARLY: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// Longer than T1 (500 ms) and well inside the give-up deadline: exactly one
/// §13.3.1.4 rung fires while the caller holds her ACK.
const HELD_ACK: Duration = Duration::from_millis(700);

/// What the callee states on its `200` and the fork answer owes the caller.
const CALLEE_STATES: &[(&str, &str)] =
    &[("Privacy", "none"), ("P-Identifier", "112233368"), ("X-Vendor-Thing", "opaque-42")];

/// The fork service: on the callee's INVITE 2xx it answers the caller under a
/// fresh A2 delivering that final, bridges the two legs, and retires.
mod fork {
    use b2bua::rules::{
        Effect, Match, RelayedFinal, RuleAction, RuleCall, RuleContext, RuleDefinition,
        RuleHandleResult, ServiceSeed, Terminal,
    };
    use b2bua::{define_service, sm_rule};
    use call::{CdrEventType, Direction, LegDisposition, LegState, TimerType};
    use sip_message::generators::SourceBody;

    define_service! {
        id: "fork",
        machine: FORK,
        states: ForkState { Waiting },
        init: |_call: &RuleCall| Some(ServiceSeed::new(ForkState::Waiting.label())),
        rules: [ answer_under_a2() ],
    }

    /// Supersedes the core `confirm-dialog`: the same confirm + bridge, the
    /// answer minted under A2 with the callee's final as the delivered one.
    fn answer_under_a2() -> RuleDefinition {
        sm_rule! {
            id: "fork-answer-under-a2",
            machine: FORK,
            active: [ ForkState::Waiting ],
            transitions: [ ForkState::Waiting => Terminal ],
            effects: [
                Effect::Respond { status: 200, label: "200 under a fresh To-tag A2, delivering the callee final" },
                Effect::GuardTimer { timer: TimerType::SetupTimeout, label: "disarm the ring and setup timers" },
                Effect::LifecycleCommand { label: "bridge caller and callee" },
            ],
            matcher: Match::response()
                .method("INVITE")
                .status_class(2)
                .direction(Direction::FromB)
                .leg_states(&[LegState::Trying, LegState::Early]),
            handle: |ctx: &RuleContext| {
                let callee_final = ctx.response()?;
                let b = ctx.source_leg_id.to_string();
                let a = ctx.call.a_leg().leg_id.clone();
                Some(RuleHandleResult::new(vec![
                    RuleAction::UpdateLegState {
                        leg_id: b.clone(),
                        state: LegState::Confirmed,
                        disposition: Some(LegDisposition::Bridged),
                    },
                    RuleAction::ConfirmDialog { leg_id: b.clone() },
                    RuleAction::AnswerALegNewDialog {
                        status: 200,
                        reason: "OK".into(),
                        body: callee_final.body().to_vec(),
                        content_type: None,
                        to_tag: None,
                        header_updates: vec![],
                        relayed: RelayedFinal::of(callee_final, SourceBody::Verbatim),
                    },
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Answer,
                        leg_id: b.clone(),
                        status_code: Some(200),
                        reason: None,
                    },
                    RuleAction::CancelTimer { id: format!("NoAnswer:{b}") },
                    RuleAction::CancelTimer { id: format!("{:?}", TimerType::SetupTimeout) },
                    RuleAction::Merge { leg_a: a, leg_b: b },
                    RuleAction::ClearState { machine: FORK },
                ]))
            },
        }
    }
}

// ── The bar ─────────────────────────────────────────────────────────────────

/// The INVITE 2xx datagrams the SUT put on the wire toward the caller, in send
/// order. Scoped to the INVITE transaction so a BYE's 200 never counts.
fn a_leg_invite_2xx(report: &RunReport, sut: SocketAddr, caller: SocketAddr) -> Vec<Vec<u8>> {
    report
        .entries()
        .iter()
        .filter(|e| e.from == sut && e.to == caller)
        .filter(|e| e.raw.starts_with(b"SIP/2.0 200 "))
        .filter(|e| cseq_method(&e.raw).as_deref() == Some("INVITE"))
        .map(|e| e.raw.clone())
        .collect()
}

/// Every callee-stated line rides the answer, named one by one.
fn assert_callee_lines_rode(lane: &str, answer: &[u8]) {
    let lines = header_lines(answer);
    for &(name, value) in CALLEE_STATES {
        assert!(
            lines.iter().any(|l| l.eq_ignore_ascii_case(&format!("{name}: {value}"))),
            "{lane}: the callee's {name} must ride the fork answer, got {lines:#?}",
        );
    }
}

/// RFC 3261 §13.3.1.4: every later copy is THE response, byte for byte.
fn assert_repeats_are_the_response(lane: &str, copies: &[Vec<u8>]) {
    assert_eq!(copies.len(), 2, "{lane}: the held ACK spans one rung — two copies");
    assert_eq!(
        String::from_utf8_lossy(&copies[1]),
        String::from_utf8_lossy(&copies[0]),
        "{lane}: the rung is not the answer it repeats",
    );
}

/// The header lines of a SIP datagram, start line and body excluded.
fn header_lines(raw: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(raw)
        .split("\r\n")
        .skip(1)
        .take_while(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// The method a datagram's `CSeq` names.
fn cseq_method(raw: &[u8]) -> Option<String> {
    header_lines(raw)
        .into_iter()
        .find(|l| l.to_ascii_lowercase().starts_with("cseq:"))
        .and_then(|l| l.split_whitespace().next_back().map(str::to_string))
}

// ── The lanes ───────────────────────────────────────────────────────────────

/// The caller ACKs at once: one answer, carrying what the callee stated, under
/// a To-tag distinct from the early dialog's.
#[tokio::test(start_paused = true)]
async fn the_fork_answer_states_what_the_callee_stated() {
    let h = Harness::new("answer-new-dialog-relays-final");
    let alice = h.agent("alice", "127.0.0.1:5341").await;
    let bob = h.agent("bob", "127.0.0.1:5351").await;
    let decision = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5351));
    let b2bua = B2buaSut::builder(decision)
        .services(vec![fork::service_def()])
        .start(&h, "b2bua", "127.0.0.1:5361")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").with_sdp(EARLY).await;
    let early = call.expect(183).await;
    let a1 = early.to().tag().expect("the 183 carries an early-dialog To-tag A1").to_string();

    let mut answer = uas.respond(200, "OK").with_sdp(ANSWER);
    for &(name, value) in CALLEE_STATES {
        answer = answer.with_header(name, value);
    }
    answer.await;
    let ok = call.expect(200).await;
    assert_ne!(ok.to().tag(), Some(a1.as_str()), "the answer forks to A2 ≠ A1 (RFC 3261 §12.1)");
    assert_eq!(ok.body(), ANSWER.as_bytes(), "the answer carries the callee's SDP");
    assert_eq!(ok.raw(HeaderName::from("Privacy")).next(), Some("none"));

    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    teardown(&b2bua, &bob, &mut dialog).await;
    let report = h.finish().await;

    let copies = a_leg_invite_2xx(&report, b2bua.addr, alice.addr());
    assert_eq!(copies.len(), 1, "the caller ACKs at once — one answer on the wire");
    assert_callee_lines_rode("fork", &copies[0]);
}

/// The caller holds her ACK past T1: the §13.3.1.4 rung repeats the answer,
/// relayed lines included.
#[tokio::test(start_paused = true)]
async fn the_fork_ladder_repeats_the_answer() {
    let h = Harness::new("answer-new-dialog-relays-final-ladder");
    let alice = h.agent("alice", "127.0.0.1:5342").await;
    let bob = h.agent("bob", "127.0.0.1:5352").await;
    let decision = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5352));
    let b2bua = B2buaSut::builder(decision)
        .services(vec![fork::service_def()])
        .start(&h, "b2bua", "127.0.0.1:5362")
        .await;

    let mut call =
        alice.invite(&bob).with_sdp(OFFER).delayed_ack(HELD_ACK).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").with_sdp(EARLY).await;
    call.expect(183).await;

    let mut answer = uas.respond(200, "OK").with_sdp(ANSWER);
    for &(name, value) in CALLEE_STATES {
        answer = answer.with_header(name, value);
    }
    answer.await;
    call.expect(200).await;

    let mut dialog = call.ack_delayed().await;
    alice.drain().await;
    bob.receive("ACK").await;
    teardown(&b2bua, &bob, &mut dialog).await;
    let report = h.finish().await;

    let copies = a_leg_invite_2xx(&report, b2bua.addr, alice.addr());
    assert_callee_lines_rode("fork-ladder", &copies[0]);
    assert_repeats_are_the_response("fork-ladder", &copies);
}

/// Caller-initiated BYE, both legs reaped.
async fn teardown(
    b2bua: &B2buaSut,
    bob: &scenario_harness::Agent,
    dialog: &mut scenario_harness::Dialog,
) {
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
}
