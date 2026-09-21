//! A leg that rings under an answered caller: no provisional reaches her.
//!
//! The caller's INVITE server transaction sent its final, and a completed
//! transaction emits no further provisional (RFC 3261 §13.3.1.1 / §17.2.1).
//! A leg this stack originates later in the call — a consult target dialled
//! by a service — rings inside a dialog the caller has confirmed: its 1xx is
//! the callee's to send and this stack's to absorb. What the ringing leg is
//! owed stays owed: its early dialog, a PRACK when the provisional is
//! reliable (RFC 3262 §4 — this stack is that leg's UAC), the CDR event, and
//! the CANCEL a later teardown sends a leg still ringing.
//!
//! The ring comes after Timer L of the answer (64·T1, RFC 6026): inside it
//! the transaction layer discards a late provisional itself, so only a later
//! one shows what the call layer does. Every strategy the caller can be under
//! is pinned: transparent relay, the bare-180 mask with every 18x relayed, and
//! the mask that acknowledges reliable provisionals itself.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::{route_to, route_to_with_18x_messages};
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut, B2buaSutBuilder};
use call::features::{Relay18xMessages, RelayFirst18xStrategy};
use call::CdrEventType;
use scenario_harness::agent::Agent;
use scenario_harness::Harness;
use sip_message::header::Supported;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const CONSULT_ANSWER: &str = "v=0\r\no=carol 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";

const CONSULT_PORT: u16 = 5090;

/// A service that dials a consult target on a deadline after the call was
/// answered, and claims nothing else: the target's responses are the core's.
mod consult {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceSeed, TimerDelay,
    };
    use b2bua::{define_service, sm_rule};
    use call::TimerType;
    use sip_message::Method;

    /// Past Timer L of the answer (64·T1 = 32 s), so the caller's INVITE
    /// server transaction is gone when the consult target rings.
    pub const DIAL_AT_SEC: i64 = 40;
    const DIAL: TimerType = TimerType::service(CONSULT, "dial");

    define_service! {
        id: "consult",
        machine: CONSULT,
        states: ConsultState { Armed, Dialled },
        init: |_call: &RuleCall| {
            Some(ServiceSeed::new(ConsultState::Armed.label()).with_actions(vec![
                RuleAction::ScheduleTimer { timer_type: DIAL, delay: TimerDelay::secs(DIAL_AT_SEC), leg_id: None },
            ]))
        },
        rules: [ dial_on_deadline() ],
    }

    fn dial_on_deadline() -> RuleDefinition {
        sm_rule! {
            id: "consult-dial",
            machine: CONSULT,
            active: [ ConsultState::Armed ],
            transitions: [ ConsultState::Armed => ConsultState::Dialled ],
            effects: [
                Effect::Originate { method: Method::Invite, label: "INVITE → consult target" },
            ],
            matcher: Match::timer().timer_type(DIAL),
            handle: |_ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    RuleAction::CreateLeg {
                        destination: ("127.0.0.1".into(), super::CONSULT_PORT),
                        new_ruri: None,
                        new_from: None,
                        new_to: None,
                        no_answer_timeout_sec: None,
                        callback_context: None,
                        body_override: None,
                        header_updates: vec![],
                        kind: None,
                    },
                    RuleAction::SetState { machine: CONSULT, to: ConsultState::Dialled.label() },
                ]))
            },
        }
    }
}

/// Route every call to bob, transparently.
fn transparent() -> B2buaSutBuilder {
    B2buaSut::builder(Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| NewCallResponse::Route(route_to("127.0.0.1", 5070)))
            .build(),
    ))
}

/// Route every call to bob under a masking strategy that relays every 18x.
fn masked(strategy: RelayFirst18xStrategy) -> B2buaSutBuilder {
    B2buaSut::builder(Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                NewCallResponse::Route(route_to_with_18x_messages(
                    "127.0.0.1",
                    5070,
                    strategy,
                    Relay18xMessages::All,
                ))
            })
            .build(),
    ))
}

/// The 1xx start-lines on `agent`'s wire after the first `200` it received.
fn provisionals_after_first_answer(agent: &Agent) -> Vec<String> {
    let lines: Vec<String> = agent.wire_view().iter().map(|e| e.start_line()).collect();
    let answered_at = lines.iter().position(|l| l.starts_with("SIP/2.0 200"));
    lines
        .iter()
        .skip(answered_at.map_or(0, |i| i + 1))
        .filter(|l| l.starts_with("SIP/2.0 1"))
        .cloned()
        .collect()
}

struct Parties {
    h: Harness,
    alice: Agent,
    bob: Agent,
    carol: Agent,
    b2bua: B2buaSut,
}

/// Bring the SUT up with the consult service and the callers' agents.
async fn start(name: &str, builder: B2buaSutBuilder) -> Parties {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", &format!("127.0.0.1:{CONSULT_PORT}")).await;
    let b2bua = builder
        .services(vec![consult::service_def()])
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    Parties { h, alice, bob, carol, b2bua }
}

/// Alice ↔ bob answered with no ringing (`reliable`: alice offers `100rel`),
/// then the consult deadline passes and carol is dialled. Returns alice's
/// dialog and carol's INVITE.
async fn answered_then_consult(
    p: &Parties,
    reliable: bool,
) -> (scenario_harness::agent::Dialog, scenario_harness::agent::ServerTxn) {
    let mut invite = p.alice.invite(&p.bob).with_sdp(OFFER).through(p.b2bua.addr);
    if reliable {
        invite = invite.with_header("Supported", "100rel");
    }
    let mut call = invite.send().await;
    let mut uas = p.bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    p.bob.receive("ACK").await;

    p.h.advance(Duration::from_secs(consult::DIAL_AT_SEC as u64 + 1)).await;
    let carol_uas = p.carol.receive("INVITE").await;
    (dialog, carol_uas)
}

/// Alice hangs up while carol still rings: bob is BYEd, carol CANCELled (the
/// leg is `Early`), and the call is reaped with its one CDR.
async fn hangup_while_ringing(
    p: &Parties,
    mut dialog: scenario_harness::agent::Dialog,
    mut carol_uas: scenario_harness::agent::ServerTxn,
) -> Vec<b2bua::cdr::CdrRecord> {
    p.h.advance(Duration::from_secs(2)).await;
    let mut bye = dialog.bye().await;
    p.bob.receive("BYE").await.respond(200, "OK").await;
    p.carol.receive("CANCEL").await.respond(200, "OK").await;
    carol_uas.respond(487, "Request Terminated").await;
    bye.expect(200).await;

    settle_until(|| p.b2bua.metrics().removals_total() == p.b2bua.metrics().creations_total())
        .await;
    p.b2bua.assert_fully_reaped();
    settle_until(|| !p.b2bua.cdr_records().is_empty()).await;
    let cdrs = p.b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR");
    assert_eq!(
        p.b2bua.metrics().provisional_after_final_refused_total(),
        0,
        "the rules absorbed the ring themselves; the a-leg seam refused nothing",
    );
    cdrs
}

/// The consult leg's provisional is on the CDR under its own leg.
fn assert_provisional_recorded(cdrs: &[b2bua::cdr::CdrRecord], status: i64) {
    assert!(
        cdrs[0].events.iter().any(|e| e.event_type == CdrEventType::Provisional
            && e.leg_id == "b-2"
            && e.status_code == Some(status)),
        "the consult leg's {status} is accounted on the CDR: {:?}",
        cdrs[0].events,
    );
}

/// Nothing provisional reached the caller after her answer.
fn assert_caller_heard_no_ring(p: &Parties) {
    let stray = provisionals_after_first_answer(&p.alice);
    assert!(
        stray.is_empty(),
        "the caller's INVITE transaction sent its final; no provisional may follow it \
         (RFC 3261 §13.3.1.1 / §17.2.1), got {stray:?}",
    );
}

// ── transparent relay ────────────────────────────────────────────────────────

/// Transparent relay: the consult target's 180 is absorbed, its leg goes
/// `Early` (the hangup CANCELs it), the CDR carries it, the caller hears nothing.
#[tokio::test(start_paused = true)]
async fn a_ringing_consult_leg_reaches_the_answered_caller_as_nothing() {
    let p = start("provisional-after-answer", transparent()).await;
    let (dialog, mut carol_uas) = answered_then_consult(&p, false).await;

    carol_uas.respond(180, "Ringing").await;

    let cdrs = hangup_while_ringing(&p, dialog, carol_uas).await;
    assert_caller_heard_no_ring(&p);
    assert_provisional_recorded(&cdrs, 180);
    let _ = p.h.finish().await;
}

/// Transparent relay, reliable provisional: the consult target's reliable
/// 183 is acknowledged by this stack (RFC 3262 §4 — the target's UAC, and
/// the caller never sees the provisional a PRACK would name), and the caller
/// hears nothing.
#[tokio::test(start_paused = true)]
async fn a_reliable_consult_provisional_is_acknowledged_by_the_stack() {
    let p = start("provisional-after-answer-reliable", transparent()).await;
    let (dialog, mut carol_uas) = answered_then_consult(&p, true).await;
    assert!(
        carol_uas
            .request()
            .header::<Supported>()
            .expect("a Supported")
            .expect("readable Supported")
            .contains("100rel"),
        "the consult INVITE offers 100rel (the caller's advertisement relayed)",
    );

    carol_uas.respond(183, "Session Progress").reliable(1).with_sdp(CONSULT_ANSWER).await;
    p.carol.receive("PRACK").await.respond(200, "OK").await;

    let cdrs = hangup_while_ringing(&p, dialog, carol_uas).await;
    assert_caller_heard_no_ring(&p);
    assert_provisional_recorded(&cdrs, 183);
    let _ = p.h.finish().await;
}

// ── the bare-180 mask ────────────────────────────────────────────────────────

/// `drop-sdp` with every 18x relayed: the original callee answered without
/// ringing, so the consult target's 180 would be "the first 18x" of the mask —
/// it is not shown to the answered caller either.
#[tokio::test(start_paused = true)]
async fn the_mask_shows_no_first_180_to_an_answered_caller() {
    let p = start("provisional-after-answer-masked", masked(RelayFirst18xStrategy::DropSdp)).await;
    let (dialog, mut carol_uas) = answered_then_consult(&p, false).await;

    carol_uas.respond(180, "Ringing").await;

    let cdrs = hangup_while_ringing(&p, dialog, carol_uas).await;
    assert_caller_heard_no_ring(&p);
    assert_provisional_recorded(&cdrs, 180);
    let _ = p.h.finish().await;
}

/// `fake-prack`: the consult INVITE offers `100rel` on this stack's own
/// behalf; the target's reliable 183 is PRACKed by the stack and shown to the
/// answered caller as nothing.
#[tokio::test(start_paused = true)]
async fn the_fake_prack_mask_acknowledges_a_consult_provisional_and_shows_nothing() {
    let p = start("provisional-after-answer-fake-prack", masked(RelayFirst18xStrategy::FakePrack))
        .await;
    let (dialog, mut carol_uas) = answered_then_consult(&p, false).await;
    assert!(
        carol_uas
            .request()
            .header::<Supported>()
            .expect("a Supported")
            .expect("readable Supported")
            .contains("100rel"),
        "fake-prack offers 100rel on the consult INVITE",
    );

    carol_uas.respond(183, "Session Progress").reliable(1).with_sdp(CONSULT_ANSWER).await;
    p.carol.receive("PRACK").await.respond(200, "OK").await;

    let cdrs = hangup_while_ringing(&p, dialog, carol_uas).await;
    assert_caller_heard_no_ring(&p);
    assert_provisional_recorded(&cdrs, 183);
    let _ = p.h.finish().await;
}
