//! The turn driver (test support): a call alice dials toward bob, driven
//! step by step through the turn as the shell runs it — the timer screen, the
//! dialog gate's verdict ([`crate::dialog_gate`]), the rule chain and the
//! landing pass — each step's SIP text built against the call as it stands
//! and against what the stack last sent each peer. The call is born as the
//! shell's initial turn folds alice's INVITE ([`Driver::dial`]), or any
//! INVITE a test writes ([`Driver::dial_invite`]).
//!
//! The driver is strict: [`Session::step`] panics on a step the call gives
//! nothing to say, and [`Session::turned`] on a step that took no turn; a
//! step declared optional goes through [`Session::try_step`]. What the gate
//! makes of a step (its answer, or a drop) and what the timer screen refuses
//! are outcomes a test reads ([`Outcome`]). A lost write is the record falling
//! back to an earlier landed one while the peers keep what they heard
//! ([`Session::lose`]).

mod dial;
mod peers;
mod steps;
mod turn;
mod txns;

use std::time::Duration;

use b2bua_sdk::event::CallEvent;
use call::features::ReferFeature;
use call::{Call, CallModelState, Direction};
use sip_ids::TurnIds;
use sip_message::{SipMessage, SipRequest, SipResponse};

use crate::answer_deadline::AnswerBudgets;
use crate::config::B2buaConfig;
use crate::decision::{default_platform_features, BodyUpdate, RouteDecision, SipDestination};
use crate::dialog_gate::{note_in_order, verdict, Verdict};
use crate::effects::{HandlerResult, OutboundBody};
use crate::landing::{land, Landing};
use crate::obligations::ObligationSet;
use crate::owners::Owners;
use crate::rule_chain::{rule_chain_turn, timer_screen, Framework, TurnInput};
use crate::rules::{compose_rules, default_rules, RuleDefinition, ServiceDef};
use crate::turn_scope::TurnScope;

pub use dial::Dialling;
pub use peers::{callee_tag, Peer, ALICE, ALICE_TAG, BOB, CALL_ID, SDP};
pub use steps::Step;
pub use turn::{Outcome, Turn};

/// The instant a driven call's first turn runs at.
pub const NOW: i64 = 1_000;

/// The per-process values every driven turn reads, and the route alice's
/// call takes.
pub struct Driver {
    config: B2buaConfig,
    rules: Vec<RuleDefinition>,
    obligations: ObligationSet,
    route: RouteDecision,
    caller_offers: bool,
    caller_lines: Vec<(&'static str, &'static str)>,
    services: Vec<ServiceDef>,
    sampled: bool,
}

impl Default for Driver {
    fn default() -> Self {
        Self::new()
    }
}

impl Driver {
    /// The worker `w0` under the default rules, routing toward bob, who may
    /// refer the call.
    pub fn new() -> Self {
        Self {
            config: B2buaConfig { self_ordinal: "w0".into(), ..Default::default() },
            rules: default_rules(),
            obligations: ObligationSet::core(),
            route: route(),
            caller_offers: true,
            caller_lines: Vec::new(),
            services: Vec::new(),
            sampled: false,
        }
    }

    /// `self`, alice's INVITE carrying no offer.
    pub fn with_offerless_caller(mut self) -> Self {
        self.caller_offers = false;
        self
    }

    /// `self`, alice's INVITE carrying the lines `lines` beside its own,
    /// `(name, value)`.
    pub fn with_caller_lines(mut self, lines: &[(&'static str, &'static str)]) -> Self {
        self.caller_lines.extend_from_slice(lines);
        self
    }

    /// `self`, its rule chain led by `rule`.
    pub fn with_rule(mut self, rule: RuleDefinition) -> Self {
        let mut rules = vec![rule];
        rules.append(&mut self.rules);
        self.rules = rules;
        self
    }

    /// `self` under the rule list `rules` alone.
    pub fn with_rules(mut self, rules: Vec<RuleDefinition>) -> Self {
        self.rules = rules;
        self
    }

    /// `self`, its configuration as `edit` leaves it.
    pub fn with_config(mut self, edit: impl FnOnce(&mut B2buaConfig)) -> Self {
        edit(&mut self.config);
        self
    }

    /// `self` with the service `def`: its rules lead the chain and its
    /// `init` seeds the call once routed, as the shell composes and seeds them
    /// (ADR-0016).
    pub fn with_service(mut self, def: ServiceDef) -> Self {
        self.rules = compose_rules(std::slice::from_ref(&def), std::mem::take(&mut self.rules));
        self.services.push(def);
        self
    }

    /// `self`, the shell's trace intake sampling every call it dials.
    pub fn sampled(mut self) -> Self {
        self.sampled = true;
        self
    }

    /// `self`, alice's route as `edit` leaves it.
    pub fn with_route(mut self, edit: impl FnOnce(&mut RouteDecision)) -> Self {
        edit(&mut self.route);
        self
    }

    /// The worker's configuration.
    pub fn config(&self) -> &B2buaConfig {
        &self.config
    }

    /// alice's INVITE, as the wire carries it.
    pub fn invite_text(&self) -> Vec<u8> {
        alice_invite_text(self.caller_offers, &self.caller_lines)
    }

    /// alice's call, routed: its b-leg `b-1` dialled toward bob.
    #[expect(clippy::panic, reason = "test support: the driver's own route dials bob")]
    pub fn dial(&self) -> Session<'_> {
        let dialling = match peers::parsed(&self.invite_text()) {
            Some(SipMessage::Request(invite)) => self.dial_invite(&invite),
            _ => Dialling::Refused,
        };
        let Dialling::Dialled(session) = dialling else { panic!("alice's INVITE dials bob") };
        *session
    }

    /// `invite` from alice, folded as the shell's initial turn folds it: the
    /// call refused, answered with its final, or dialled toward bob.
    pub fn dial_invite(&self, invite: &SipRequest) -> Dialling<'_> {
        let Some(routed) = dial::fold(self, invite) else { return Dialling::Refused };
        let Some(b_invite) = last_request_to(&routed, "b-1") else {
            return Dialling::Final(Box::new(routed));
        };
        Dialling::Dialled(Box::new(Session {
            driver: self,
            call: routed.call,
            peers: peers::Peers::new(&b_invite),
            owners: Owners::default(),
            now: NOW,
            steps: 0,
            outcomes: Vec::new(),
            texts: Vec::new(),
            txns: txns::ClientTxns::of(&b_invite),
        }))
    }
}

/// A driven call: its record, its peers, its clock and what each step came
/// to.
#[derive(Clone)]
pub struct Session<'d> {
    driver: &'d Driver,
    call: Call,
    peers: peers::Peers,
    owners: Owners,
    now: i64,
    steps: u64,
    outcomes: Vec<Outcome>,
    texts: Vec<(usize, Vec<u8>)>,
    /// What this node's transaction layer holds of the requests sent.
    txns: txns::ClientTxns,
}

impl<'d> Session<'d> {
    /// The record as the last turn landed it.
    pub fn call(&self) -> &Call {
        &self.call
    }

    /// The call's clock.
    pub fn now(&self) -> i64 {
        self.now
    }

    /// The peer on `leg`, if the stack reached it.
    pub fn peer(&self, leg: &str) -> Option<&Peer> {
        self.peers.on(leg)
    }

    /// What each step came to, in order.
    pub fn outcomes(&self) -> &[Outcome] {
        &self.outcomes
    }

    /// Every turn taken, in order.
    pub fn turns(&self) -> impl Iterator<Item = &Turn> {
        self.outcomes.iter().filter_map(Outcome::turn)
    }

    /// The requests the turns sent, `leg METHOD`, in order.
    pub fn sent(&self) -> Vec<String> {
        self.turns()
            .flat_map(|t| t.result.effects.outbound.iter())
            .filter_map(|e| match &e.body {
                OutboundBody::Request(r) => {
                    Some(format!("{} {}", e.leg_id.as_deref()?, r.method()))
                }
                _ => None,
            })
            .collect()
    }

    /// The responses the turns sent, by leg, in order.
    pub fn responses(&self) -> Vec<(String, SipResponse)> {
        self.turns()
            .flat_map(|t| t.result.effects.outbound.iter())
            .filter_map(|e| match &e.body {
                OutboundBody::Response(r) => Some((e.leg_id.clone()?, r.clone())),
                _ => None,
            })
            .collect()
    }

    /// The defects the turns noted, `step N: name: detail`.
    pub fn defects(&self) -> Vec<String> {
        let mut found = Vec::new();
        for (n, outcome) in self.outcomes.iter().enumerate() {
            let Some(turn) = outcome.turn() else { continue };
            for d in turn.notes.defects() {
                found.push(format!("step {n}: {}: {}", d.name, d.detail));
            }
        }
        found
    }

    /// The turns' owners: whether another owner served the call, read by
    /// the gate and the evidence stage (ADR-0055 §4).
    pub fn copy_served(&mut self, served: bool) -> &mut Self {
        self.owners.copy_served = served;
        self
    }

    /// Run `script` strictly on a copy that served the call, whose writes are
    /// lost: the record falls back to the one it held before, the peers keep
    /// what they heard, and no client transaction of those turns' requests is
    /// this node's.
    pub fn lose(&mut self, script: impl IntoIterator<Item = Step>) -> &mut Self {
        let record = self.call.clone();
        let from = self.outcomes.len();
        self.run(script);
        for turn in self.outcomes.iter().skip(from).filter_map(Outcome::turn) {
            self.txns.lost(&turn.result);
        }
        self.call = record;
        self
    }

    /// The record replaced by `record`, whatever this session landed: a
    /// snapshot test's state no event sequence lands.
    pub fn revert_to(&mut self, record: Call) -> &mut Self {
        self.call = record;
        self
    }

    /// Run every step of `script`, strictly.
    pub fn run(&mut self, script: impl IntoIterator<Item = Step>) -> &mut Self {
        for step in script {
            self.step(step);
        }
        self
    }

    /// Run `step`; panics when the call gives it nothing to say.
    #[expect(clippy::panic, reason = "test support: a strict step does something")]
    pub fn step(&mut self, step: Step) -> &Outcome {
        let state = self.call.state;
        let outcome = self.try_step(step.clone());
        if matches!(outcome, Outcome::Nothing) {
            panic!("{step:?} finds nothing to do on the call in {state:?}");
        }
        outcome
    }

    /// Run `step`, which must take a turn, and return it.
    #[expect(clippy::panic, reason = "test support: a strict step takes the turn it expects")]
    pub fn turned(&mut self, step: Step) -> &Turn {
        match self.step(step.clone()) {
            Outcome::Turned(turn) => turn,
            other => panic!("{step:?} takes no turn: {other:?}"),
        }
    }

    /// Run `step`, declared optional: it may find nothing to do.
    pub fn try_step(&mut self, step: Step) -> &Outcome {
        self.step_edited(step, &|raw| raw.to_vec())
    }

    /// Run `step` with its SIP text rewritten by `edit`; it may find
    /// nothing to do.
    #[expect(clippy::expect_used, reason = "test support: the outcome was just pushed")]
    pub fn step_edited(&mut self, step: Step, edit: &dyn Fn(&[u8]) -> Vec<u8>) -> &Outcome {
        let outcome = self.outcome_of(&step, edit);
        self.outcomes.push(outcome);
        self.outcomes.last().expect("the step's outcome")
    }

    /// The SIP text each SIP step the timer screen let through reached the
    /// call as, by its step's index.
    pub fn texts(&self) -> &[(usize, Vec<u8>)] {
        &self.texts
    }

    fn outcome_of(&mut self, step: &Step, edit: &dyn Fn(&[u8]) -> Vec<u8>) -> Outcome {
        let seq = self.steps;
        self.steps += 1;
        if let Step::Advance(ms) = step {
            self.now += ms;
            return Outcome::Advanced;
        }
        if self.call.state == CallModelState::Terminated {
            return Outcome::Nothing;
        }
        let Some((event, leg)) = steps::event_of(step, &self.call, &mut self.peers, self.now)
        else {
            return Outcome::Nothing;
        };
        let Some(event) = steps::edited(event, edit) else { return Outcome::Unparsed };
        if self.txns.absorbs(&event) {
            return Outcome::Absorbed;
        }
        self.txns.received(&event);
        self.now = self.now.max(deadline(&self.call, &event));
        let mut turn = TurnScope::of(TurnIds::fixed(seq + 1));
        if timer_screen::screen(&mut self.call, &event, self.now, &mut turn.notes).is_err() {
            return Outcome::Screened;
        }
        if let Some(text) = steps::text_of(&event) {
            self.texts.push((self.outcomes.len(), text));
        }
        let served = || self.owners.copy_served;
        let resolved = match verdict(&self.call, &leg, &event, &self.driver.config, served) {
            Verdict::Pass(resolved) => resolved,
            refused => {
                if let Some(in_order) = refused.in_order() {
                    self.call = note_in_order(self.call.clone(), &leg, in_order, &event);
                }
                return Outcome::Gated(refused);
            }
        };
        if steps::is_cancel(&event) {
            return Outcome::StrayCancel;
        }
        let turn = self.turn(event, &leg, resolved, turn);
        self.call = turn.result.call.clone();
        self.peers.heard(&turn.result);
        self.txns.sent(&turn.result);
        Outcome::Turned(Box::new(turn))
    }

    /// The turn `event` arriving on `leg` takes, resolved as `resolved`.
    fn turn(
        &self,
        event: CallEvent,
        leg: &str,
        resolved: call::helpers::Resolved,
        mut scope: TurnScope,
    ) -> Turn {
        let driver = self.driver;
        let fw = Framework {
            config: &driver.config,
            rules: &driver.rules,
            obligations: &driver.obligations,
        };
        let gated = note_in_order(self.call.clone(), leg, &resolved, &event);
        let call_ref = gated.call_ref.clone();
        let input = TurnInput {
            event: &event,
            call_ref: &call_ref,
            source_leg_id: leg,
            direction: if leg == "a" { Direction::FromA } else { Direction::FromB },
            resolved: resolved.clone(),
            matched_client_txn: self.txns.matches(&event),
            now_ms: self.now,
            owners: self.owners.clone(),
        };
        let result = land(
            rule_chain_turn(&fw, gated, input, &mut scope),
            &landing(&driver.config),
            self.now,
        );
        Turn { result, rule: scope.handled_by, notes: scope.notes, resolved }
    }
}

/// The deadline of the ledger entry `event` fires, or the dawn of time for
/// any other event: a fire happens at its deadline.
fn deadline(call: &Call, event: &CallEvent) -> i64 {
    let CallEvent::Timer { key, generation, .. } = event else { return i64::MIN };
    let entry = call.timers.iter().find(|t| t.key == *key && t.generation == *generation);
    entry.map_or(i64::MIN, |t| t.fire_at)
}

/// The landing pass every driven turn takes.
pub(super) fn landing(config: &B2buaConfig) -> Landing<'_> {
    Landing {
        config,
        budgets: AnswerBudgets {
            http: Duration::from_secs(5),
            admit: Duration::from_secs(1),
            failure: Some(Duration::from_secs(5)),
            release: Some(Duration::from_secs(5)),
        },
        refresh_period: Duration::from_secs(60),
    }
}

/// alice's INVITE toward bob, with an offer when `offers`, carrying `lines`
/// after her Contact.
fn alice_invite_text(offers: bool, lines: &[(&str, &str)]) -> Vec<u8> {
    let (typed, body) = if offers { ("Content-Type: application/sdp\r\n", SDP) } else { ("", "") };
    let stated: String = lines.iter().map(|(name, value)| format!("{name}: {value}\r\n")).collect();
    format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP {ALICE};branch=z9hG4bK-lane\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag={ALICE_TAG}\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: {CALL_ID}\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:alice@{ALICE}>\r\n\
         {stated}\
         {typed}Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// alice's route toward bob, a consult on its failure, bob allowed to refer.
fn route() -> RouteDecision {
    let mut features = default_platform_features();
    features.refer = Some(ReferFeature { max_chain_depth: None });
    RouteDecision {
        destination: SipDestination::new("127.0.0.1", 5070),
        new_ruri: Some("sip:bob@127.0.0.1:5070".into()),
        new_from: None,
        new_to: None,
        update_headers: None,
        update_body: BodyUpdate::Keep,
        no_answer_ms: Some(30_000),
        call_limiter: Vec::new(),
        callback_context: Some("lane-context".into()),
        features,
        service_ext: Default::default(),
        subscriptions: Vec::new(),
        trace: false,
        label: None,
    }
}

/// The last request other than an ACK `result` sends on `leg`.
fn last_request_to(result: &HandlerResult, leg: &str) -> Option<SipRequest> {
    result.effects.outbound.iter().rev().find_map(|e| match &e.body {
        OutboundBody::Request(r)
            if e.leg_id.as_deref() == Some(leg) && *r.method() != sip_message::Method::Ack =>
        {
            Some(r.clone())
        }
        _ => None,
    })
}

#[cfg(test)]
#[path = "strict_tests.rs"]
mod strict_tests;
