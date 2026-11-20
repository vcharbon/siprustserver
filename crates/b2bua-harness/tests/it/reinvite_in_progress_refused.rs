//! RFC 3261 §14.1: a UAC MUST NOT initiate an INVITE transaction on a dialog
//! while another INVITE transaction is in progress on it, in either direction.
//! A rule asking for one makes the stack refuse it: the INVITE is not sent and
//! the turn reports the defect `one-invite-in-progress-per-dialog`, naming the
//! rule refused and the transaction in progress. Each scenario takes that one
//! defect and checks the wire carried the one INVITE the dialog allows.
//!
//! - two re-INVITEs asked on the caller's dialog in one turn;
//! - a re-INVITE asked toward the callee while the callee's own re-INVITE
//!   awaits its final;
//! - a re-INVITE asked between the 2xx to this stack's re-INVITE and its ACK;
//! - a peer's re-INVITE a rule relays over the glare refusal onto a dialog
//!   whose re-INVITE awaits its final: the sender gets 491 (§14.2);
//! - a re-INVITE asked toward a peer whose re-INVITE this stack answered 2xx,
//!   ahead of the peer's ACK.

use std::time::Duration;

use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const DEFECT: &str = "one-invite-in-progress-per-dialog";

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
/// Each party's next description of its session (RFC 3264 §8).
const ALICE_2: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";
const BOB_2: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";

/// When each service's renegotiation fires, after the call is set up.
const RENEGOTIATE_SEC: u64 = 4;

/// The pieces every renegotiating service shares.
mod shared {
    use b2bua::rules::{RuleAction, RuleContext, ServiceSeed, StateLabel, TimerDelay};
    use b2bua_sdk::model::Body;
    use call::{TimerKey, TimerType};

    /// The service seeded in `armed`, its `timer` firing [`RENEGOTIATE_SEC`]
    /// after the call starts.
    ///
    /// [`RENEGOTIATE_SEC`]: super::RENEGOTIATE_SEC
    pub fn arm(armed: StateLabel, timer: TimerType) -> Option<ServiceSeed> {
        Some(ServiceSeed::new(armed).with_actions(vec![RuleAction::arm(
            TimerKey::call(timer),
            TimerDelay::secs(super::RENEGOTIATE_SEC as i64),
        )]))
    }

    /// The callee's leg.
    pub fn callee(ctx: &RuleContext) -> Option<String> {
        ctx.call.b_legs().first().map(|leg| leg.leg_id.clone())
    }

    /// A re-INVITE toward the caller offering the callee's next description.
    pub fn reinvite_caller(ctx: &RuleContext) -> Option<RuleAction> {
        let b = callee(ctx)?;
        Some(RuleAction::SendReinvite {
            leg_id: "a".into(),
            body: Some(Body::from_leg(super::BOB_2.as_bytes().to_vec(), b)),
            add_headers: vec![],
        })
    }

    /// A re-INVITE toward the callee offering the caller's next description.
    pub fn reinvite_callee(ctx: &RuleContext) -> Option<RuleAction> {
        let b = callee(ctx)?;
        Some(RuleAction::SendReinvite {
            leg_id: b,
            body: Some(Body::from_leg(super::ALICE_2.as_bytes().to_vec(), "a")),
            add_headers: vec![],
        })
    }
}

/// On its timer, asks for two re-INVITEs toward the caller in one turn; the
/// caller's 2xx is ACKed.
mod pair {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        Terminal,
    };
    use b2bua::{define_service, sm_rule};
    use call::{Direction, TimerType};
    use sip_message::Method;

    const RENEGOTIATE: TimerType = TimerType::service(PAIR, "renegotiate");

    define_service! {
        id: "pair",
        machine: PAIR,
        states: PairState { Armed, Renegotiating },
        init: |_call: &RuleCall| super::shared::arm(PairState::Armed.label(), RENEGOTIATE),
        rules: [ fire(), answered() ],
    }

    fn fire() -> RuleDefinition {
        sm_rule! {
            id: "pair-fire",
            machine: PAIR,
            active: [ PairState::Armed ],
            transitions: [ PairState::Armed => PairState::Renegotiating ],
            effects: [
                Effect::Originate { method: Method::Invite, label: "re-INVITE → caller, twice" },
            ],
            matcher: Match::timer().timer_type(RENEGOTIATE),
            handle: |ctx: &RuleContext| {
                let first = super::shared::reinvite_caller(ctx)?;
                Some(RuleHandleResult::new(vec![
                    first.clone(),
                    first,
                    RuleAction::SetState { machine: PAIR, to: PairState::Renegotiating.label() },
                ]))
            },
        }
    }

    fn answered() -> RuleDefinition {
        sm_rule! {
            id: "pair-answered",
            machine: PAIR,
            active: [ PairState::Renegotiating ],
            transitions: [ PairState::Renegotiating => Terminal ],
            effects: [ Effect::Originate { method: Method::Ack, label: "ACK → caller" } ],
            matcher: Match::response().method("INVITE").status_class(2).direction(Direction::FromA),
            handle: |_ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    RuleAction::AckLeg { leg_id: "a".into(), body: None },
                    RuleAction::ClearState { machine: PAIR },
                ]))
            },
        }
    }
}

/// On its timer, asks for a re-INVITE toward the callee.
mod toward_callee {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        Terminal,
    };
    use b2bua::{define_service, sm_rule};
    use call::TimerType;
    use sip_message::Method;

    const RENEGOTIATE: TimerType = TimerType::service(CALLEE_SIDE, "renegotiate");

    define_service! {
        id: "callee-side",
        machine: CALLEE_SIDE,
        states: CalleeState { Armed },
        init: |_call: &RuleCall| super::shared::arm(CalleeState::Armed.label(), RENEGOTIATE),
        rules: [ fire() ],
    }

    fn fire() -> RuleDefinition {
        sm_rule! {
            id: "callee-side-fire",
            machine: CALLEE_SIDE,
            active: [ CalleeState::Armed ],
            transitions: [ CalleeState::Armed => Terminal ],
            effects: [ Effect::Originate { method: Method::Invite, label: "re-INVITE → callee" } ],
            matcher: Match::timer().timer_type(RENEGOTIATE),
            handle: |ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    super::shared::reinvite_callee(ctx)?,
                    RuleAction::ClearState { machine: CALLEE_SIDE },
                ]))
            },
        }
    }
}

/// On its timer, re-INVITEs the caller; on her 2xx asks for a second
/// re-INVITE before the ACK of the first.
mod before_ack {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        Terminal,
    };
    use b2bua::{define_service, sm_rule};
    use call::{Direction, TimerType};
    use sip_message::Method;

    const RENEGOTIATE: TimerType = TimerType::service(BEFORE_ACK, "renegotiate");

    define_service! {
        id: "before-ack",
        machine: BEFORE_ACK,
        states: BeforeAckState { Armed, Renegotiating },
        init: |_call: &RuleCall| super::shared::arm(BeforeAckState::Armed.label(), RENEGOTIATE),
        rules: [ fire(), answered() ],
    }

    fn fire() -> RuleDefinition {
        sm_rule! {
            id: "before-ack-fire",
            machine: BEFORE_ACK,
            active: [ BeforeAckState::Armed ],
            transitions: [ BeforeAckState::Armed => BeforeAckState::Renegotiating ],
            effects: [ Effect::Originate { method: Method::Invite, label: "re-INVITE → caller" } ],
            matcher: Match::timer().timer_type(RENEGOTIATE),
            handle: |ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    super::shared::reinvite_caller(ctx)?,
                    RuleAction::SetState {
                        machine: BEFORE_ACK,
                        to: BeforeAckState::Renegotiating.label(),
                    },
                ]))
            },
        }
    }

    fn answered() -> RuleDefinition {
        sm_rule! {
            id: "before-ack-answered",
            machine: BEFORE_ACK,
            active: [ BeforeAckState::Renegotiating ],
            transitions: [ BeforeAckState::Renegotiating => Terminal ],
            effects: [
                Effect::Originate { method: Method::Invite, label: "re-INVITE → caller, again" },
                Effect::Originate { method: Method::Ack, label: "ACK → caller" },
            ],
            matcher: Match::response().method("INVITE").status_class(2).direction(Direction::FromA),
            handle: |ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    super::shared::reinvite_caller(ctx)?,
                    RuleAction::AckLeg { leg_id: "a".into(), body: None },
                    RuleAction::ClearState { machine: BEFORE_ACK },
                ]))
            },
        }
    }
}

/// On its timer, re-INVITEs the caller and ACKs her 2xx; meanwhile a rule of
/// its own relays the callee's re-INVITE, out-ranking the glare refusal.
mod over_glare {
    use b2bua::rules::{
        Effect, Match, MessageTransform, RuleAction, RuleCall, RuleContext, RuleDefinition,
        RuleHandleResult, Terminal, SERVICE_LAYER,
    };
    use b2bua::{define_service, sm_rule};
    use call::{Direction, TimerType};
    use sip_message::Method;

    const RENEGOTIATE: TimerType = TimerType::service(OVER_GLARE, "renegotiate");

    define_service! {
        id: "over-glare",
        machine: OVER_GLARE,
        states: OverGlareState { Armed, Renegotiating },
        init: |_call: &RuleCall| super::shared::arm(OverGlareState::Armed.label(), RENEGOTIATE),
        rules: [ fire(), answered(), relay_callee_reinvite() ],
    }

    fn fire() -> RuleDefinition {
        sm_rule! {
            id: "over-glare-fire",
            machine: OVER_GLARE,
            active: [ OverGlareState::Armed ],
            transitions: [ OverGlareState::Armed => OverGlareState::Renegotiating ],
            effects: [ Effect::Originate { method: Method::Invite, label: "re-INVITE → caller" } ],
            matcher: Match::timer().timer_type(RENEGOTIATE),
            handle: |ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    super::shared::reinvite_caller(ctx)?,
                    RuleAction::SetState {
                        machine: OVER_GLARE,
                        to: OverGlareState::Renegotiating.label(),
                    },
                ]))
            },
        }
    }

    fn answered() -> RuleDefinition {
        sm_rule! {
            id: "over-glare-answered",
            machine: OVER_GLARE,
            active: [ OverGlareState::Renegotiating ],
            transitions: [ OverGlareState::Renegotiating => Terminal ],
            effects: [ Effect::Originate { method: Method::Ack, label: "ACK → caller" } ],
            matcher: Match::response().method("INVITE").status_class(2).direction(Direction::FromA),
            handle: |_ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    RuleAction::AckLeg { leg_id: "a".into(), body: None },
                    RuleAction::ClearState { machine: OVER_GLARE },
                ]))
            },
        }
    }

    fn relay(_ctx: &RuleContext) -> Option<RuleHandleResult> {
        Some(RuleHandleResult::new(vec![RuleAction::RelayToPeer {
            transform: MessageTransform::default(),
        }]))
    }

    /// The callee's re-INVITE relayed whatever is open: the rule shape the
    /// relay's own guard holds against.
    fn relay_callee_reinvite() -> RuleDefinition {
        RuleDefinition {
            id: "over-glare-relay",
            layer: SERVICE_LAYER,
            overrides: &["reinvite-glare", "relay-reinvite"],
            matcher: Match::request().method("INVITE").direction(Direction::FromB),
            handle: relay,
            machine: None,
            active_states: &[],
            transitions: &[],
            effects: &[],
            teardown: false,
        }
    }
}

/// Relays the caller's 2xx to a callee's re-INVITE and, in the same turn,
/// asks for a re-INVITE toward that callee.
mod after_answer {
    use b2bua::rules::{
        Match, MessageTransform, RuleAction, RuleCall, RuleContext, RuleDefinition,
        RuleHandleResult, ServiceDef, ServiceSeed, SERVICE_LAYER,
    };
    use call::Direction;

    fn relay_then_reinvite(ctx: &RuleContext) -> Option<RuleHandleResult> {
        Some(RuleHandleResult::new(vec![
            RuleAction::RelayToPeer { transform: MessageTransform::default() },
            super::shared::reinvite_callee(ctx)?,
        ]))
    }

    fn rules() -> Vec<RuleDefinition> {
        vec![RuleDefinition {
            id: "after-answer-relay",
            layer: SERVICE_LAYER,
            overrides: &["relay-reinvite-response"],
            matcher: Match::response().method("INVITE").status_class(2).direction(Direction::FromA),
            handle: relay_then_reinvite,
            machine: None,
            active_states: &[],
            transitions: &[],
            effects: &[],
            teardown: false,
        }]
    }

    fn init(_call: &RuleCall) -> Option<ServiceSeed> {
        None
    }

    pub fn service_def() -> ServiceDef {
        ServiceDef { id: "after-answer", init, rules }
    }
}

/// The run's one defect, naming the rule refused and, in `holder`'s words,
/// what holds the dialog.
fn the_one_defect(b2bua: &mut B2buaSut, rule: &str, holder: &str) {
    let violations = b2bua.take_model_violations();
    assert_eq!(violations.len(), 1, "the refusal is reported once: {violations:?}");
    let head = violations.first().and_then(|v| v.lines().next()).unwrap_or_default();
    assert!(head.starts_with(&format!("model defect {DEFECT}: rule '{rule}'")), "{head}");
    assert!(head.ends_with(&format!("not sent: {holder}")), "{head}");
    assert_eq!(b2bua.metrics().model_defects_total(DEFECT), 1, "the refusal is counted");
}

/// A call set up and answered: alice's dialog, bob's dialog.
macro_rules! answered_call {
    ($alice:expr, $bob:expr, $b2bua:expr) => {{
        let mut call = $alice.invite(&$bob).with_sdp(OFFER).through($b2bua.addr).send().await;
        let mut uas = $bob.receive("INVITE").await;
        uas.respond(200, "OK").with_sdp(ANSWER).await;
        call.expect(200).await;
        let alice_dialog = call.ack().await;
        $bob.receive("ACK").await;
        (alice_dialog, uas.dialog())
    }};
}

/// Two re-INVITEs asked on the caller's dialog in one turn: the first leaves,
/// the second is refused while the first awaits its final. The caller's 2xx is
/// ACKed and the call ends normally.
#[tokio::test(start_paused = true)]
async fn a_second_reinvite_in_the_same_turn_is_refused() {
    let h = Harness::with_transit_delay("reinvite-in-progress-pair", 0);
    let alice = h.agent("alice", "127.0.0.1:5160").await;
    let bob = h.agent("bob", "127.0.0.1:5170").await;
    let mut b2bua = B2buaSut::route_all_to("127.0.0.1", 5170)
        .services(vec![pair::service_def()])
        .start(&h, "b2bua", "127.0.0.1:5180")
        .await;
    let (mut alice_dialog, _bob_dialog) = answered_call!(alice, bob, b2bua);

    h.advance(Duration::from_secs(RENEGOTIATE_SEC)).await;
    let mut reinvite = alice.receive("INVITE").await;
    let cseq = reinvite.request().cseq().seq();
    settle_until(|| b2bua.metrics().model_defects_total(DEFECT) == 1).await;
    assert!(alice.try_receive_tolerating("INVITE", &[]).await.is_none(), "one INVITE leaves");
    reinvite.respond(200, "OK").with_sdp(ALICE_2).await;
    alice.receive("ACK").await;

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    let holder = format!("INVITE CSeq {cseq} sent by rule 'pair-fire' awaits its final or its ACK");
    the_one_defect(&mut b2bua, "pair-fire", &holder);
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// The callee re-INVITEs and the caller holds her final; a re-INVITE asked
/// toward the callee then is refused, since the callee's own INVITE is in
/// progress on that dialog. The callee's renegotiation completes untouched.
#[tokio::test(start_paused = true)]
async fn a_reinvite_toward_a_peer_whose_reinvite_is_unanswered_is_refused() {
    let h = Harness::with_transit_delay("reinvite-in-progress-peer", 0);
    let alice = h.agent("alice", "127.0.0.1:5161").await;
    let bob = h.agent("bob", "127.0.0.1:5171").await;
    let mut b2bua = B2buaSut::route_all_to("127.0.0.1", 5171)
        .services(vec![toward_callee::service_def()])
        .start(&h, "b2bua", "127.0.0.1:5181")
        .await;
    let (mut alice_dialog, mut bob_dialog) = answered_call!(alice, bob, b2bua);

    let mut bob_reinvite = bob_dialog.request(InDialogMethod::Invite, Some(BOB_2)).await;
    bob_reinvite.expect(100).await;
    let mut relayed = alice.receive("INVITE").await;
    h.advance(Duration::from_secs(RENEGOTIATE_SEC)).await;
    settle_until(|| b2bua.metrics().model_defects_total(DEFECT) == 1).await;
    assert!(bob.try_receive_tolerating("INVITE", &[]).await.is_none(), "no INVITE toward bob");

    relayed.respond(200, "OK").with_sdp(ALICE_2).await;
    bob_reinvite.expect(200).await;
    bob_dialog.ack(None).await;
    alice.receive("ACK").await;

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    the_one_defect(&mut b2bua, "callee-side-fire", "the peer's INVITE has no final");
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// The caller answers this stack's re-INVITE 2xx; a second re-INVITE asked in
/// that turn, ahead of the ACK, is refused: only the ACK reaches her.
#[tokio::test(start_paused = true)]
async fn a_reinvite_before_the_ack_of_the_last_2xx_is_refused() {
    let h = Harness::with_transit_delay("reinvite-in-progress-before-ack", 0);
    let alice = h.agent("alice", "127.0.0.1:5162").await;
    let bob = h.agent("bob", "127.0.0.1:5172").await;
    let mut b2bua = B2buaSut::route_all_to("127.0.0.1", 5172)
        .services(vec![before_ack::service_def()])
        .start(&h, "b2bua", "127.0.0.1:5182")
        .await;
    let (mut alice_dialog, _bob_dialog) = answered_call!(alice, bob, b2bua);

    h.advance(Duration::from_secs(RENEGOTIATE_SEC)).await;
    let mut reinvite = alice.receive("INVITE").await;
    let cseq = reinvite.request().cseq().seq();
    reinvite.respond(200, "OK").with_sdp(ALICE_2).await;
    alice.receive("ACK").await;
    settle_until(|| b2bua.metrics().model_defects_total(DEFECT) == 1).await;
    assert!(alice.try_receive_tolerating("INVITE", &[]).await.is_none(), "only the ACK leaves");

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    let holder =
        format!("INVITE CSeq {cseq} sent by rule 'before-ack-fire' awaits its final or its ACK");
    the_one_defect(&mut b2bua, "before-ack-answered", &holder);
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// This stack's re-INVITE to the caller awaits her final; the callee
/// re-INVITEs and a rule relays it over the glare refusal. The relay does not
/// leave: the callee gets 491 (§14.2) and the caller sees one INVITE.
#[tokio::test(start_paused = true)]
async fn a_relay_onto_a_dialog_with_an_invite_in_progress_is_491ed() {
    let h = Harness::with_transit_delay("reinvite-in-progress-relay", 0);
    let alice = h.agent("alice", "127.0.0.1:5163").await;
    let bob = h.agent("bob", "127.0.0.1:5173").await;
    let mut b2bua = B2buaSut::route_all_to("127.0.0.1", 5173)
        .services(vec![over_glare::service_def()])
        .start(&h, "b2bua", "127.0.0.1:5183")
        .await;
    let (mut alice_dialog, mut bob_dialog) = answered_call!(alice, bob, b2bua);

    h.advance(Duration::from_secs(RENEGOTIATE_SEC)).await;
    let mut reinvite = alice.receive("INVITE").await;
    let cseq = reinvite.request().cseq().seq();
    let mut bob_reinvite = bob_dialog.request(InDialogMethod::Invite, Some(BOB_2)).await;
    bob_reinvite.expect(491).await;
    assert!(alice.try_receive_tolerating("INVITE", &[]).await.is_none(), "one INVITE to alice");

    reinvite.respond(200, "OK").with_sdp(ALICE_2).await;
    alice.receive("ACK").await;

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    let holder =
        format!("INVITE CSeq {cseq} sent by rule 'over-glare-fire' awaits its final or its ACK");
    the_one_defect(&mut b2bua, "over-glare-relay", &holder);
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// The callee's re-INVITE is answered by the caller and her 2xx relayed to
/// him; a re-INVITE asked toward the callee in that turn, ahead of his ACK, is
/// refused: the 2xx this stack sent him awaits that ACK.
#[tokio::test(start_paused = true)]
async fn a_reinvite_toward_a_peer_owing_the_ack_of_its_2xx_is_refused() {
    let h = Harness::with_transit_delay("reinvite-in-progress-unacked", 0);
    let alice = h.agent("alice", "127.0.0.1:5164").await;
    let bob = h.agent("bob", "127.0.0.1:5174").await;
    let mut b2bua = B2buaSut::route_all_to("127.0.0.1", 5174)
        .services(vec![after_answer::service_def()])
        .start(&h, "b2bua", "127.0.0.1:5184")
        .await;
    let (mut alice_dialog, mut bob_dialog) = answered_call!(alice, bob, b2bua);

    let mut bob_reinvite = bob_dialog.request(InDialogMethod::Invite, Some(BOB_2)).await;
    let cseq = bob_reinvite.sent_invite().expect("bob's re-INVITE").cseq().seq();
    alice.receive("INVITE").await.respond(200, "OK").with_sdp(ALICE_2).await;
    bob_reinvite.expect(200).await;
    settle_until(|| b2bua.metrics().model_defects_total(DEFECT) == 1).await;
    assert!(bob.try_receive_tolerating("INVITE", &[]).await.is_none(), "no INVITE toward bob");
    bob_dialog.ack(None).await;
    alice.receive("ACK").await;

    let mut bye = alice_dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    let holder = format!("2xx to the peer's INVITE CSeq {cseq} sent, its ACK not received");
    the_one_defect(&mut b2bua, "after-answer-relay", &holder);
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}
