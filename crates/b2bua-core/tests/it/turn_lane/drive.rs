//! A call dialled toward bob and the turns the lane runs over it: the
//! caller's INVITE folded through the decision's route as the shell folds it,
//! then each step of a script built against the call as it stands and
//! against what the stack last sent each peer, its SIP text mutated when the
//! case says so, resolved to the dialog it names as the shell's dialog gate
//! resolves it, then run through the timer screen, the rule chain and the
//! landing pass as the shell runs them.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_core::answer_deadline::{AnswerBudgets, CONSULT_CHANGE};
use b2bua_core::config::B2buaConfig;
use b2bua_core::decision::route_fold::{self, Admitting};
use b2bua_core::decision::{default_platform_features, BodyUpdate, RouteDecision, SipDestination};
use b2bua_core::effects::{HandlerResult, OutboundBody};
use b2bua_core::initial_invite::{build_initial_call, reject_call};
use b2bua_core::landing::{land, Landing};
use b2bua_core::obligations::ObligationSet;
use b2bua_core::rule_chain::{rule_chain_turn, timer_screen, Framework, TurnInput};
use b2bua_core::rules::invariants::{enforce, finalize, UnansweredCaller};
use b2bua_core::rules::lifecycle_delivery::{close_turn, TurnStart};
use b2bua_core::rules::relay::rebuild_a_leg_invite;
use b2bua_core::rules::{default_rules, ActionExecutor, RuleDefinition};
use b2bua_core::turn_scope::TurnScope;
use b2bua_sdk::event::CallEvent;
use call::features::{
    ReferFeature, Relay18xMessages, RelayFirst18xStrategy, RelayFirst18xTo180Feature,
};
use call::helpers::{response_origin, Resolved, ResponseFacts, ResponseOrigin};
use call::{Call, CallModelState, Direction, TerminationCause, TimerType};
use sip_ids::TurnIds;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipHeader, SipMessage, SipParser, SipRequest, SipResponse};
use wire_mutations::{mutate, Mutation};

/// The instant every turn runs at.
pub const NOW: i64 = 1_000;

const SDP: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "10.0.0.9:5060";
const BOB: &str = "127.0.0.1:5070";
const CAROL: &str = "127.0.0.1:5071";

/// The leg the stack dials bob on, and the one a transfer dials carol on.
const BOB_LEG: &str = "b-1";
const CAROL_LEG: &str = "b-2";

/// One step of a script: what reaches the call next.
#[derive(Clone, Debug)]
pub enum Step {
    /// bob's response to the b-leg INVITE.
    BobAnswers(u16),
    /// bob's response to the b-leg INVITE carrying these lines: a 2xx or
    /// 18x carries the answer.
    BobAnswersWith(u16, &'static [(&'static str, &'static str)]),
    /// bob's reliable provisional to the b-leg INVITE, carrying the answer
    /// (RFC 3262).
    BobAnswersReliably(u16),
    /// bob's response to the last request the stack sent him.
    BobAnswersSent(u16),
    /// bob's response to the last request of this method the stack sent him.
    BobAnswersLast(Method, u16),
    /// A fork's response to the b-leg INVITE, under the fork's own To-tag
    /// (RFC 3261 §12.1.2).
    ForkAnswers(&'static str, u16),
    /// That fork's reliable provisional (RFC 3262).
    ForkAnswersReliably(&'static str, u16),
    /// bob's BYE on the dialog his 2xx confirmed.
    BobByes,
    /// bob's REFER on that dialog (RFC 3515).
    BobRefers,
    /// alice's response to the last request the stack sent her.
    AliceAnswersSent(u16),
    /// carol's response to the INVITE a transfer dialled her with.
    CarolAnswers(u16),
    /// carol's response to the last request the stack sent her.
    CarolAnswersSent(u16),
    /// alice's request of this method on her dialog: a re-INVITE or an
    /// UPDATE carries an offer.
    AliceSends(Method),
    /// alice's ACK of the 2xx to her last INVITE.
    AliceAcks,
    /// alice's BYE on her dialog.
    AliceByes,
    /// alice's CANCEL of her INVITE, which the transaction layer answered.
    AliceCancels,
    /// A fire of the call's timer of this type, at the generation it holds.
    Fires(TimerType),
    /// The `/call/failure` consult's answer, `outcome` with `payload`.
    FailureAnswers { outcome: &'static str, payload: serde_json::Value },
    /// The `/call/refer` consult's answer, `outcome` with `payload`.
    ReferAnswers { outcome: &'static str, payload: serde_json::Value },
}

/// What a call is dialled under: alice's INVITE and the route the decision
/// answers it with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setup {
    /// alice's offer routed toward bob, who may refer the call.
    Plain,
    /// alice's INVITE asks for a session timer (RFC 4028).
    SessionTimer,
    /// bob's early media masked from alice under `strategy`
    /// (`relay_first_18x_to_180`), every 18x relayed downgraded.
    EarlyMedia(RelayFirst18xStrategy),
}

/// A script's call, dialled toward bob.
pub struct Dialled {
    pub call: Call,
    pub b_invite: SipRequest,
    /// The per-process values every turn reads, built once.
    config: B2buaConfig,
    rules: Vec<RuleDefinition>,
    obligations: ObligationSet,
}

impl Dialled {
    /// The dialled call, its rule chain led by `rule`.
    pub fn with_rule(mut self, rule: RuleDefinition) -> Self {
        self.rules.insert(0, rule);
        self
    }
}

/// The worker's configuration.
pub fn config() -> B2buaConfig {
    B2buaConfig { self_ordinal: "w0".into(), ..Default::default() }
}

fn parsed(raw: &[u8]) -> Option<SipMessage> {
    CustomParser::new().parse(raw).ok()
}

/// alice's INVITE under `setup`, as the wire carries it.
pub fn alice_invite_text(setup: Setup) -> Vec<u8> {
    let timer = match setup {
        Setup::SessionTimer => {
            "Supported: timer\r\nSession-Expires: 1800;refresher=uac\r\nMin-SE: 90\r\n"
        }
        Setup::Plain | Setup::EarlyMedia(_) => "",
    };
    format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP {ALICE};branch=z9hG4bK-lane\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=alicetag\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: lane@10.0.0.9\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:alice@{ALICE}>\r\n\
         {timer}\
         Content-Type: application/sdp\r\n\
         Content-Length: {}\r\n\r\n{SDP}",
        SDP.len()
    )
    .into_bytes()
}

/// The INVITE request `raw` reads as, if any.
pub fn invite_of(raw: &[u8]) -> Option<SipRequest> {
    match parsed(raw)? {
        SipMessage::Request(r) if *r.method() == Method::Invite => Some(r),
        _ => None,
    }
}

/// The route the decision answers alice's call with under `setup`.
pub fn route(setup: Setup) -> RouteDecision {
    let mut features = default_platform_features();
    features.refer = Some(ReferFeature { max_chain_depth: None });
    if let Setup::EarlyMedia(strategy) = setup {
        features.relay_first_18x_to_180 =
            Some(RelayFirst18xTo180Feature { strategy, messages: Relay18xMessages::All });
    }
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

/// alice's plain call, routed toward bob, who may refer it.
pub fn dialled() -> Dialled {
    dialled_with(Setup::Plain)
}

/// alice's call under `setup`, routed toward bob.
pub fn dialled_with(setup: Setup) -> Dialled {
    let invite = invite_of(&alice_invite_text(setup)).expect("alice's INVITE parses");
    match dial(&invite, route(setup)) {
        Dialling::Dialled(dialled) => *dialled,
        other => panic!("{setup:?}: the route dials bob: {other:?}"),
    }
}

/// What the shell makes of a caller's INVITE the decision answers with a
/// route.
#[derive(Debug)]
pub enum Dialling {
    /// The call is never born: its record does not rebuild the INVITE
    /// (ADR-0052 §11).
    Refused,
    /// The call's final, as the turn landed it: the INVITE carries no From
    /// tag (RFC 3261 §8.1.1.3), or the route was refused.
    Final(Box<HandlerResult>),
    /// bob's leg dialled.
    Dialled(Box<Dialled>),
}

impl std::fmt::Debug for Dialled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dialled").field("b_invite", &self.b_invite).finish_non_exhaustive()
    }
}

/// `invite` from alice folded through `route` as the shell's initial turn
/// folds it: the call built, its record's rebuild of the INVITE, the call
/// refused 400 without a From tag or the route applied up to its admit and
/// admitted, the turn finalized, its obligations enforced, its lifecycle
/// delivered and the turn landed. The trace intake and the services' seeds
/// are left out: the lane runs no tracing and no service.
pub fn dial(invite: &SipRequest, route: RouteDecision) -> Dialling {
    let config = config();
    let rules = default_rules();
    let owners = b2bua_core::owners::Owners::none();
    let src: SocketAddr = ALICE.parse().expect("an address");
    let mut turn = TurnScope::of(TurnIds::fixed(0));
    let call = build_initial_call(invite, src, &config, &mut turn.ids, 0);
    let Ok(caller) = rebuild_a_leg_invite(&call.a_leg_invite) else { return Dialling::Refused };
    let born = call.clone();
    let ids = &mut turn.ids;
    let routed = if call.a_leg_invite.from_tag.is_empty() {
        let reason = Some("Bad Request - missing From tag".into());
        let advertisement = &config.minted_final_advertisement;
        let cause = TerminationCause::Admission;
        reject_call(call, &caller, 400, reason, None, &[], advertisement, ids, 0, cause)
    } else {
        match route_fold::up_to_admit(call, route, &caller, &config, ids, 0, 0) {
            Admitting::Final(result) => *result,
            Admitting::Admit(admitting) => {
                route_fold::admitted(*admitting, &caller, &config, owners, ids, 0, 0)
            }
        }
    };
    let obligations = ObligationSet::core();
    let unanswered = UnansweredCaller::Answer(&config.minted_final_advertisement);
    let enforced = enforce(&obligations, &born, finalize(routed), 0, unanswered, &mut turn.notes);
    let mut exec = ActionExecutor { config: &config, owners, turn: &mut turn, now_ms: 0 };
    let closed = close_turn(&rules, &TurnStart::unborn(), enforced, &mut exec);
    let routed = land(closed, &landing(&config), 0);
    let Some(b_invite) = sent_to(&routed, BOB_LEG) else {
        return Dialling::Final(Box::new(routed));
    };
    Dialling::Dialled(Box::new(Dialled { call: routed.call, b_invite, config, rules, obligations }))
}

/// The landing pass's values, as the shell computes them.
fn landing(config: &B2buaConfig) -> Landing<'_> {
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

/// The last request other than an ACK `result` sends on `leg`.
fn sent_to(result: &HandlerResult, leg: &str) -> Option<SipRequest> {
    result.effects.outbound.iter().rev().find_map(|e| match &e.body {
        OutboundBody::Request(r)
            if e.leg_id.as_deref() == Some(leg) && *r.method() != Method::Ack =>
        {
            Some(r.clone())
        }
        _ => None,
    })
}

/// What the peers have sent and been sent so far in a run.
struct Peers {
    /// alice's last CSeq number, and her last INVITE's.
    alice_cseq: u32,
    alice_invite_cseq: u32,
    /// bob's last CSeq number on his dialog.
    bob_cseq: u32,
    /// The last request other than an ACK the stack sent alice, bob and
    /// carol.
    to_alice: Option<SipRequest>,
    to_bob: Option<SipRequest>,
    to_carol: Option<SipRequest>,
    /// The INVITE that dialled carol.
    carol_invite: Option<SipRequest>,
    /// Every request other than an ACK the stack sent bob, in order.
    bob_heard: Vec<SipRequest>,
}

impl Peers {
    fn new(dialled: &Dialled) -> Self {
        Self {
            alice_cseq: 1,
            alice_invite_cseq: 1,
            bob_cseq: 1,
            to_alice: None,
            to_bob: Some(dialled.b_invite.clone()),
            to_carol: None,
            carol_invite: None,
            bob_heard: vec![dialled.b_invite.clone()],
        }
    }

    /// The requests `result` sends, kept as the last each peer was sent.
    fn heard(&mut self, result: &HandlerResult) {
        self.to_alice = sent_to(result, "a").or(self.to_alice.take());
        self.to_bob = sent_to(result, BOB_LEG).or(self.to_bob.take());
        let to_carol = sent_to(result, CAROL_LEG);
        if self.carol_invite.is_none() {
            self.carol_invite = to_carol.clone().filter(|r| *r.method() == Method::Invite);
        }
        self.to_carol = to_carol.or(self.to_carol.take());
        self.bob_heard.extend(result.effects.outbound.iter().filter_map(|e| match &e.body {
            OutboundBody::Request(r)
                if e.leg_id.as_deref() == Some(BOB_LEG) && *r.method() != Method::Ack =>
            {
                Some(r.clone())
            }
            _ => None,
        }));
    }
}

/// A request of `method` from `src`, its lines `lines` before the body.
fn request(method: &str, cseq: u32, src: &str, lines: &str, body: &str) -> Vec<u8> {
    format!(
        "{method} sip:svc@10.0.0.1:5060 SIP/2.0\r\n\
         Via: SIP/2.0/UDP {src};branch=z9hG4bK-lane-{method}-{cseq}\r\n\
         Max-Forwards: 70\r\n\
         {lines}\
         CSeq: {cseq} {method}\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// The response `status` to `req`, its To-tag `bobtag` when it has none,
/// a 2xx to an INVITE or an UPDATE carrying the answer.
fn response(req: &SipRequest, status: u16, extra: Vec<SipHeader>) -> Option<Vec<u8>> {
    response_tagged(req, status, "bobtag", extra)
}

/// [`response`] under the To-tag `tag` when `req` has none.
fn response_tagged(
    req: &SipRequest,
    status: u16,
    tag: &str,
    extra: Vec<SipHeader>,
) -> Option<Vec<u8>> {
    let answers = matches!(req.method(), Method::Invite | Method::Update)
        && ((200..300).contains(&status) || (180..200).contains(&status));
    let opts = GenerateResponseOpts {
        to_tag: req.to().tag().is_none().then(|| tag.into()),
        body: if answers { SDP.into() } else { Vec::new() },
        content_type: answers.then(b2bua_core::rules::relay::sdp),
        extra_headers: extra,
        ..Default::default()
    };
    Some(generate_response(req, status, "Lane", &opts).ok()?.image().to_vec())
}

/// The lines making a provisional reliable (RFC 3262 §3).
fn reliable() -> Vec<SipHeader> {
    vec![line("Require", "100rel"), line("RSeq", "1")]
}

fn lines(stated: &[(&str, &str)]) -> Vec<SipHeader> {
    stated.iter().map(|(name, value)| line(name, value)).collect()
}

fn line(name: &str, value: &str) -> SipHeader {
    SipHeader { name: name.to_string().into(), value: value.to_string().into() }
}

/// The SIP text `raw` arriving from `leg`'s peer.
fn sip(raw: Vec<u8>, leg: &'static str) -> Option<(CallEvent, &'static str)> {
    let peer = match leg {
        "a" => ALICE,
        CAROL_LEG => CAROL,
        _ => BOB,
    };
    let src = peer.parse().ok()?;
    Some((CallEvent::sip(parsed(&raw)?, src), leg))
}

/// The step as it reaches the call standing as `call`: the event and the
/// leg it arrived on, or `None` when the call gives the step nothing to say
/// (no dialog to say it on, nothing sent to answer, no timer of the type
/// armed).
fn event_of(
    step: &Step,
    call: &Call,
    b_invite: &SipRequest,
    peers: &mut Peers,
) -> Option<(CallEvent, &'static str)> {
    let alice_lines = |own: &str| {
        format!(
            "From: <sip:alice@example.com>;tag=alicetag\r\n\
             To: <sip:bob@example.com>;tag={own}\r\n\
             Call-ID: lane@10.0.0.9\r\n"
        )
    };
    let bob_lines = || -> Option<String> {
        Some(format!(
            "From: <sip:bob@{BOB}>;tag=bobtag\r\n\
             To: <sip:alice@example.com>;tag={}\r\n\
             Call-ID: {}\r\n",
            b_invite.from().tag()?,
            b_invite.call_id().as_str()
        ))
    };
    match step {
        Step::BobAnswers(status) => sip(response(b_invite, *status, Vec::new())?, BOB_LEG),
        Step::BobAnswersWith(status, stated) => {
            sip(response(b_invite, *status, lines(stated))?, BOB_LEG)
        }
        Step::BobAnswersReliably(status) => sip(response(b_invite, *status, reliable())?, BOB_LEG),
        Step::BobAnswersSent(status) => {
            sip(response(peers.to_bob.as_ref()?, *status, Vec::new())?, BOB_LEG)
        }
        Step::BobAnswersLast(method, status) => {
            let req = peers.bob_heard.iter().rev().find(|r| r.method() == method)?;
            sip(response(req, *status, Vec::new())?, BOB_LEG)
        }
        Step::ForkAnswers(tag, status) => {
            sip(response_tagged(b_invite, *status, tag, Vec::new())?, BOB_LEG)
        }
        Step::ForkAnswersReliably(tag, status) => {
            sip(response_tagged(b_invite, *status, tag, reliable())?, BOB_LEG)
        }
        Step::AliceAnswersSent(status) => {
            sip(response(peers.to_alice.as_ref()?, *status, Vec::new())?, "a")
        }
        Step::CarolAnswers(status) => {
            let invite = peers.carol_invite.as_ref()?;
            sip(response_tagged(invite, *status, "caroltag", Vec::new())?, CAROL_LEG)
        }
        Step::CarolAnswersSent(status) => {
            let req = peers.to_carol.as_ref()?;
            sip(response_tagged(req, *status, "caroltag", Vec::new())?, CAROL_LEG)
        }
        Step::BobByes => {
            peers.bob_cseq += 1;
            sip(request("BYE", peers.bob_cseq, BOB, &bob_lines()?, ""), BOB_LEG)
        }
        Step::BobRefers => {
            peers.bob_cseq += 1;
            let lines = format!(
                "{}Contact: <sip:bob@{BOB}>\r\nRefer-To: <sip:carol@127.0.0.1:5071>\r\n",
                bob_lines()?
            );
            sip(request("REFER", peers.bob_cseq, BOB, &lines, ""), BOB_LEG)
        }
        Step::AliceSends(method) => {
            let own = call.a_leg.own_tag.as_deref()?;
            peers.alice_cseq += 1;
            let offers = matches!(method, Method::Invite | Method::Update);
            if *method == Method::Invite {
                peers.alice_invite_cseq = peers.alice_cseq;
            }
            let mut lines = alice_lines(own);
            if offers {
                lines.push_str(&format!(
                    "Contact: <sip:alice@{ALICE}>\r\nContent-Type: application/sdp\r\n"
                ));
            }
            let body = if offers { SDP } else { "" };
            sip(request(method.as_str(), peers.alice_cseq, ALICE, &lines, body), "a")
        }
        Step::AliceAcks => {
            let own = call.a_leg.own_tag.as_deref()?;
            sip(request("ACK", peers.alice_invite_cseq, ALICE, &alice_lines(own), ""), "a")
        }
        Step::AliceByes => {
            let own = call.a_leg.own_tag.as_deref()?;
            peers.alice_cseq += 1;
            sip(request("BYE", peers.alice_cseq, ALICE, &alice_lines(own), ""), "a")
        }
        Step::AliceCancels => {
            let event = CallEvent::Cancelled {
                call_id: "lane@10.0.0.9".into(),
                from_tag: "alicetag".into(),
                invite_cseq: Some(1),
                in_dialog: false,
                headers: Vec::new(),
                to_tag: call.a_leg.own_tag.clone(),
            };
            Some((event, "a"))
        }
        Step::Fires(timer_type) => {
            let armed = call.timers.iter().find(|t| t.timer_type() == timer_type)?;
            let event = CallEvent::Timer {
                key: armed.key.clone(),
                generation: armed.generation,
                call_ref: call.call_ref.clone(),
                incarnation: None,
            };
            let leg = match armed.key.leg_id() {
                None | Some("a") => "a",
                Some(CAROL_LEG) => CAROL_LEG,
                Some(_) => BOB_LEG,
            };
            Some((event, leg))
        }
        Step::FailureAnswers { outcome, payload } => {
            let change = call.timers.iter().find_map(|t| match t.timer_type() {
                TimerType::FailureAnswer { change, .. } => Some(*change),
                _ => None,
            })?;
            let mut payload = payload.clone();
            if let Some(map) = payload.as_object_mut() {
                map.insert(CONSULT_CHANGE.into(), change.into());
            }
            Some((internal(call, "call-failure-result", outcome, payload), "a"))
        }
        Step::ReferAnswers { outcome, payload } => {
            Some((internal(call, "refer-http-result", outcome, payload.clone()), BOB_LEG))
        }
    }
}

/// An internal event of `topic` for `call`.
fn internal(call: &Call, topic: &str, outcome: &str, payload: serde_json::Value) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref: call.call_ref.clone(),
        topic: topic.into(),
        outcome: outcome.into(),
        payload,
        body: Vec::new(),
        incarnation: None,
    }
}

/// What the shell's dialog gate makes of `event` arriving on `leg`: the
/// dialog it names and the call with its remote sequence noted, or `None`
/// when the gate keeps it from the rules (a request naming no dialog, a
/// response answering nothing of the leg's).
fn resolve(call: Call, leg: &str, event: &CallEvent) -> Option<(Resolved, Call)> {
    let CallEvent::Sip { message, .. } = event else { return Some((Resolved::default(), call)) };
    match message.as_ref() {
        SipMessage::Response(resp) => {
            let cseq = resp.cseq();
            let facts = ResponseFacts {
                to: resp.to().tag(),
                from: resp.from().tag(),
                cseq: i64::from(cseq.seq()),
                method: cseq.method().as_str(),
                status: resp.status(),
                branch: resp.top_via().branch(),
            };
            match response_origin(&call, leg, facts) {
                ResponseOrigin::Dialog(resolved) => Some((resolved, call)),
                ResponseOrigin::Creates | ResponseOrigin::Request => {
                    Some((Resolved::default(), call))
                }
                ResponseOrigin::Stray => None,
            }
        }
        SipMessage::Request(req) => {
            let Some(to) = req.to().tag() else { return Some((Resolved::default(), call)) };
            let found = call::helpers::find_leg(&call, leg)?;
            let dialled = found.outbound_invite().is_some();
            let dialog = found
                .dialogs
                .of_request(Some(to), req.from().tag())
                .filter(|d| dialled || d.shown || d.was_established())?;
            let id = dialog.id();
            let cseq = i64::from(req.cseq().seq());
            if *req.method() == Method::Ack {
                return Some((Resolved { dialog: Some(id), record: None }, call));
            }
            if dialog.peer_cseq.is_some_and(|last| cseq < last) {
                return None;
            }
            let call = call::helpers::advance_peer_cseq(call, leg, &id, cseq);
            Some((Resolved { dialog: Some(id), record: None }, call))
        }
    }
}

/// The SIP text of `event` rewritten by `edit`, read back; `None` when the
/// parser refuses it, as the shell drops what does not parse.
fn edited(event: CallEvent, edit: &dyn Fn(&[u8]) -> Vec<u8>) -> Option<CallEvent> {
    let CallEvent::Sip { message, src } = event else {
        return Some(event);
    };
    Some(CallEvent::sip(parsed(&edit(message.image()))?, src))
}

/// Run `script` over the dialled call, the step `mutated_at` taking
/// `mutations` to its SIP text: each turn as the shell runs it — the timer
/// screen, the dialog gate's resolution, the rule chain, the landing pass.
/// Each turn's input is fixed, so two runs of one case compare.
pub fn run(dialled: &Dialled, script: &[Step], mutated_at: usize, mutations: &[Mutation]) -> Run {
    let edit = |raw: &[u8]| mutations.iter().fold(raw.to_vec(), |acc, m| mutate::apply(&acc, m));
    run_from(dialled, dialled.call.clone(), script, mutated_at, &edit)
}

/// [`run`] with the SIP text of step `at` replaced whole by `text`.
pub fn run_replacing(dialled: &Dialled, script: &[Step], at: usize, text: &[u8]) -> Run {
    run_from(dialled, dialled.call.clone(), script, at, &|_| text.to_vec())
}

/// [`run`] over `call`, the dialled call as an earlier script left it.
pub fn run_on(dialled: &Dialled, call: Call, script: &[Step]) -> Run {
    run_from(dialled, call, script, usize::MAX, &|raw| raw.to_vec())
}

fn run_from(
    dialled: &Dialled,
    mut call: Call,
    script: &[Step],
    edited_at: usize,
    edit: &dyn Fn(&[u8]) -> Vec<u8>,
) -> Run {
    let config = &dialled.config;
    let fw = Framework { config, rules: &dialled.rules, obligations: &dialled.obligations };
    let landing = landing(config);
    let mut peers = Peers::new(dialled);
    let mut run = Run {
        turns: Vec::new(),
        raws: Vec::new(),
        sent: Vec::new(),
        responses: Vec::new(),
        defects: Vec::new(),
        call: call.clone(),
    };
    for (n, step) in script.iter().enumerate() {
        if call.state == CallModelState::Terminated {
            break;
        }
        let Some((event, leg)) = event_of(step, &call, &dialled.b_invite, &mut peers) else {
            continue;
        };
        let event = if n == edited_at { edited(event, edit) } else { Some(event) };
        let Some(event) = event else { continue };
        let mut turn = TurnScope::of(TurnIds::fixed(n as u64 + 1));
        if timer_screen::screen(&mut call, &event, NOW, &mut turn.notes).is_err() {
            continue;
        }
        if let CallEvent::Sip { message, .. } = &event {
            run.raws.push((n, message.image().to_vec()));
        }
        let Some((resolved, gated)) = resolve(call.clone(), leg, &event) else { continue };
        let input = TurnInput {
            event: &event,
            call_ref: &gated.call_ref.clone(),
            source_leg_id: leg,
            direction: if leg == "a" { Direction::FromA } else { Direction::FromB },
            resolved,
            // Every response a peer sends answers a request the stack sent.
            matched_client_txn: matches!(&event, CallEvent::Sip { message, .. }
                if matches!(message.as_ref(), SipMessage::Response(_))),
            now_ms: NOW,
            owners: Default::default(),
        };
        let result: HandlerResult =
            land(rule_chain_turn(&fw, gated, input, &mut turn), &landing, NOW);
        peers.heard(&result);
        run.sent.extend(result.effects.outbound.iter().filter_map(|e| match &e.body {
            OutboundBody::Request(r) => Some(format!("{} {}", e.leg_id.as_deref()?, r.method())),
            _ => None,
        }));
        run.responses.extend(result.effects.outbound.iter().filter_map(|e| match &e.body {
            OutboundBody::Response(r) => Some((e.leg_id.clone()?, r.clone())),
            _ => None,
        }));
        run.defects.extend(
            turn.notes.defects().iter().map(|d| format!("step {n}: {}: {}", d.name, d.detail)),
        );
        run.turns.push(format!("{result:?} {:?}", turn.notes));
        call = result.call;
    }
    run.call = call;
    run
}

/// What a script's run left: each turn's result as text, the SIP text each
/// SIP step reached the call as (by step), the requests the turns sent (`leg
/// METHOD`) and the responses, by leg, the defects its turns noted, and the
/// call.
pub struct Run {
    pub turns: Vec<String>,
    pub raws: Vec<(usize, Vec<u8>)>,
    pub sent: Vec<String>,
    pub responses: Vec<(String, SipResponse)>,
    pub defects: Vec<String>,
    pub call: Call,
}
