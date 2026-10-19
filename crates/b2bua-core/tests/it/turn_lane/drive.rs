//! A call dialled toward bob and the turns the lane runs over it: each step
//! of a script built against the call as it stands, its SIP text mutated
//! when the case says so, run through the timer screen, the rule chain and
//! the landing pass as the shell runs them.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_core::answer_deadline::{AnswerBudgets, CONSULT_CHANGE};
use b2bua_core::config::B2buaConfig;
use b2bua_core::decision::route_fold::{self, Admitting};
use b2bua_core::decision::{default_platform_features, BodyUpdate, RouteDecision, SipDestination};
use b2bua_core::effects::{HandlerResult, OutboundBody};
use b2bua_core::initial_invite::build_initial_call;
use b2bua_core::landing::{land, Landing};
use b2bua_core::obligations::ObligationSet;
use b2bua_core::rule_chain::{rule_chain_turn, timer_screen, Framework, TurnInput};
use b2bua_core::rules::default_rules;
use b2bua_core::turn_scope::TurnScope;
use b2bua_sdk::event::CallEvent;
use call::{Call, CallModelState, Direction, TimerType};
use sip_ids::TurnIds;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};
use wire_mutations::{mutate, Mutation};

/// The instant every turn runs at.
pub const NOW: i64 = 1_000;

const SDP: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// One step of a script: what reaches the call next.
#[derive(Clone, Debug)]
pub enum Step {
    /// bob's response to the b-leg INVITE.
    BobAnswers(u16),
    /// bob's BYE on the dialog his 2xx confirmed.
    BobByes,
    /// alice's BYE on her dialog.
    AliceByes,
    /// A fire of the call's timer of this type, at the generation it holds.
    Fires(TimerType),
    /// The `/call/failure` consult's answer, `outcome` with `payload`.
    FailureAnswers { outcome: &'static str, payload: serde_json::Value },
}

/// The call the lane starts every script from: alice's INVITE routed toward
/// bob, its b-leg dialled.
pub struct Dialled {
    pub call: Call,
    pub b_invite: SipRequest,
}

/// The worker's configuration.
pub fn config() -> B2buaConfig {
    B2buaConfig { self_ordinal: "w0".into(), ..Default::default() }
}

fn parsed(raw: &[u8]) -> Option<SipMessage> {
    CustomParser::new().parse(raw).ok()
}

fn alice_invite() -> SipRequest {
    let raw = format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-lane\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=alicetag\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: lane@10.0.0.9\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:alice@10.0.0.9:5060>\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {}\r\n\r\n{SDP}",
        SDP.len()
    );
    match parsed(raw.as_bytes()) {
        Some(SipMessage::Request(r)) => r,
        other => panic!("alice's INVITE parses: {other:?}"),
    }
}

fn route() -> RouteDecision {
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
        features: default_platform_features(),
        service_ext: Default::default(),
        subscriptions: Vec::new(),
        trace: false,
        label: None,
    }
}

/// alice's call, routed toward bob.
pub fn dialled() -> Dialled {
    let config = config();
    let invite = alice_invite();
    let src: SocketAddr = "10.0.0.9:5060".parse().expect("an address");
    let mut ids = TurnIds::fixed(0);
    let call = build_initial_call(&invite, src, &config, &mut ids, 0);
    let caller = crate::caller_of(&call);
    let Admitting::Admit(admitting) =
        route_fold::up_to_admit(call, route(), &caller, &config, &mut ids, 0, 0)
    else {
        panic!("the route stands");
    };
    let routed = route_fold::admitted(*admitting, &caller, &config, &mut ids, 0, 0);
    let b_invite = routed
        .effects
        .outbound
        .iter()
        .find_map(|e| match &e.body {
            OutboundBody::Request(r) => Some(r.clone()),
            _ => None,
        })
        .expect("the b-leg INVITE");
    Dialled { call: routed.call, b_invite }
}

/// The step as it reaches the call standing as `call`: the event and the
/// leg it arrived on, or `None` when the call gives the step nothing to say
/// (no dialog to say it on, no timer of the type armed).
fn event_of(step: &Step, call: &Call, b_invite: &SipRequest) -> Option<(CallEvent, &'static str)> {
    let sip = |raw: Vec<u8>, leg: &'static str| {
        let message = Box::new(parsed(&raw)?);
        let src = if leg == "a" { "10.0.0.9:5060" } else { "127.0.0.1:5070" };
        let src = src.parse().ok()?;
        Some((CallEvent::Sip { message, src, matched_client_txn: leg != "a" }, leg))
    };
    match step {
        Step::BobAnswers(status) => {
            let opts = GenerateResponseOpts {
                to_tag: Some("bobtag".into()),
                body: if (180..300).contains(status) { SDP.into() } else { Vec::new() },
                content_type: (180..300).contains(status).then(b2bua_core::rules::relay::sdp),
                ..Default::default()
            };
            let response = generate_response(b_invite, *status, "Lane", &opts).ok()?;
            sip(response.image().to_vec(), "b-1")
        }
        Step::BobByes => {
            let from_tag = b_invite.from().tag()?;
            let raw = format!(
                "BYE sip:svc@10.0.0.1:5060 SIP/2.0\r\n\
                 Via: SIP/2.0/UDP 127.0.0.1:5070;branch=z9hG4bK-lane-bob-bye\r\n\
                 Max-Forwards: 70\r\n\
                 From: <sip:bob@127.0.0.1:5070>;tag=bobtag\r\n\
                 To: <sip:alice@example.com>;tag={from_tag}\r\n\
                 Call-ID: {}\r\n\
                 CSeq: 2 BYE\r\n\
                 Content-Length: 0\r\n\r\n",
                b_invite.call_id().as_str()
            );
            sip(raw.into_bytes(), "b-1")
        }
        Step::AliceByes => {
            let own = call.a_leg.own_tag.as_deref()?;
            let raw = format!(
                "BYE sip:svc@10.0.0.1:5060 SIP/2.0\r\n\
                 Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-lane-alice-bye\r\n\
                 Max-Forwards: 70\r\n\
                 From: <sip:alice@example.com>;tag=alicetag\r\n\
                 To: <sip:bob@example.com>;tag={own}\r\n\
                 Call-ID: lane@10.0.0.9\r\n\
                 CSeq: 2 BYE\r\n\
                 Content-Length: 0\r\n\r\n"
            );
            sip(raw.into_bytes(), "a")
        }
        Step::Fires(timer_type) => {
            let armed = call.timers.iter().find(|t| t.timer_type() == timer_type)?;
            let event = CallEvent::Timer {
                key: armed.key.clone(),
                generation: armed.generation,
                call_ref: call.call_ref.clone(),
                incarnation: None,
            };
            Some((event, armed.key.leg_id().map_or("a", |_| "b-1")))
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
            let event = CallEvent::InternalEvent {
                call_ref: call.call_ref.clone(),
                topic: "call-failure-result".into(),
                outcome: (*outcome).into(),
                payload,
                body: Vec::new(),
                incarnation: None,
            };
            Some((event, "a"))
        }
    }
}

/// The SIP text of `event` with `mutations` applied, read back; `None` when
/// the parser refuses it, as the shell drops what does not parse.
fn mutated(event: CallEvent, mutations: &[Mutation]) -> Option<CallEvent> {
    let CallEvent::Sip { message, src, matched_client_txn } = event else {
        return Some(event);
    };
    let raw = mutations.iter().fold(message.image().to_vec(), |acc, m| mutate::apply(&acc, m));
    Some(CallEvent::Sip { message: Box::new(parsed(&raw)?), src, matched_client_txn })
}

/// Run `script` over the dialled call, the step `mutated_at` taking
/// `mutations` to its SIP text: each turn as the shell runs it — the timer
/// screen, the rule chain, the landing pass. Each turn's input is fixed, so
/// two runs of one case compare.
pub fn run(dialled: &Dialled, script: &[Step], mutated_at: usize, mutations: &[Mutation]) -> Run {
    let config = config();
    let rules = default_rules();
    let obligations = ObligationSet::core();
    let fw = Framework { config: &config, rules: &rules, obligations: &obligations };
    let landing = Landing {
        config: &config,
        budgets: AnswerBudgets {
            http: Duration::from_secs(5),
            admit: Duration::from_secs(1),
            failure: Some(Duration::from_secs(5)),
            release: Some(Duration::from_secs(5)),
        },
        refresh_period: Duration::from_secs(60),
    };
    let mut call = dialled.call.clone();
    let (mut turns, mut raws) = (Vec::new(), Vec::new());
    for (n, step) in script.iter().enumerate() {
        if call.state == CallModelState::Terminated {
            break;
        }
        let Some((event, leg)) = event_of(step, &call, &dialled.b_invite) else { continue };
        let event = if n == mutated_at { mutated(event, mutations) } else { Some(event) };
        let Some(event) = event else { continue };
        let mut turn = TurnScope::of(TurnIds::fixed(n as u64 + 1));
        if timer_screen::screen(&mut call, &event, NOW, &mut turn.notes).is_err() {
            continue;
        }
        let caller = crate::caller_of(&call);
        let input = TurnInput {
            event: &event,
            call_ref: &call.call_ref.clone(),
            source_leg_id: leg,
            direction: if leg == "a" { Direction::FromA } else { Direction::FromB },
            resolved: Default::default(),
            now_ms: NOW,
            caller: &caller,
        };
        if let CallEvent::Sip { message, .. } = &event {
            raws.push((n, message.image().to_vec()));
        }
        let result: HandlerResult =
            land(rule_chain_turn(&fw, call, input, &mut turn), &landing, NOW);
        turns.push(format!("{result:?} {:?}", turn.notes));
        call = result.call;
    }
    Run { turns, raws, call }
}

/// What a script's run left: each turn's result as text, the SIP text each
/// SIP step reached the call as (by step), and the call.
pub struct Run {
    pub turns: Vec<String>,
    pub raws: Vec<(usize, Vec<u8>)>,
    pub call: Call,
}
