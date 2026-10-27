//! The property lane's runs over the shared turn driver
//! ([`b2bua_core::turn_driver`]): the setups a script's call is dialled
//! under, then a script run from the dialled call with every step optional —
//! an edited sequence or a mutated text may leave a step nothing to say —
//! one step's SIP text mutated when the case says so.

use b2bua_core::turn_driver::{Driver, Session, Step};
use call::features::{Relay18xMessages, RelayFirst18xStrategy, RelayFirst18xTo180Feature};
use call::{Call, CallModelState};
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser, SipRequest, SipResponse};
use wire_mutations::{mutate, Mutation};

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

/// alice's session timer, she its refresher (RFC 4028 §7.1).
const SESSION_TIMER: &[(&str, &str)] =
    &[("Supported", "timer"), ("Session-Expires", "1800;refresher=uac"), ("Min-SE", "90")];

/// The driver that dials alice's call under `setup`.
pub fn driver(setup: Setup) -> Driver {
    match setup {
        Setup::Plain => Driver::new(),
        Setup::SessionTimer => Driver::new().with_caller_lines(SESSION_TIMER),
        Setup::EarlyMedia(strategy) => Driver::new().with_route(|route| {
            route.features.relay_first_18x_to_180 =
                Some(RelayFirst18xTo180Feature { strategy, messages: Relay18xMessages::All });
        }),
    }
}

/// alice's INVITE under `setup`, as the wire carries it.
pub fn alice_invite_text(setup: Setup) -> Vec<u8> {
    driver(setup).invite_text()
}

/// The INVITE request `raw` reads as, if any.
pub fn invite_of(raw: &[u8]) -> Option<SipRequest> {
    match CustomParser::new().parse(raw).ok()? {
        SipMessage::Request(r) if *r.method() == Method::Invite => Some(r),
        _ => None,
    }
}

/// The INVITE that dialled bob on `dialled`.
pub fn bob_invite<'s>(dialled: &'s Session) -> &'s SipRequest {
    let bob = dialled.peer("b-1").and_then(|bob| bob.dialled_by.as_ref());
    bob.expect("a dialled call has dialled bob")
}

/// Run `script` over the dialled call, the step `mutated_at` taking
/// `mutations` to its SIP text. Each turn's input is fixed, so two runs of
/// one case compare.
pub fn run(dialled: &Session, script: &[Step], mutated_at: usize, mutations: &[Mutation]) -> Run {
    let edit = |raw: &[u8]| mutations.iter().fold(raw.to_vec(), |acc, m| mutate::apply(&acc, m));
    run_from(dialled, script, mutated_at, &edit)
}

/// [`run`] with the SIP text of step `at` replaced whole by `text`.
pub fn run_replacing(dialled: &Session, script: &[Step], at: usize, text: &[u8]) -> Run {
    run_from(dialled, script, at, &|_| text.to_vec())
}

fn run_from(
    dialled: &Session,
    script: &[Step],
    edited_at: usize,
    edit: &dyn Fn(&[u8]) -> Vec<u8>,
) -> Run {
    let mut session = dialled.clone();
    for (n, step) in script.iter().enumerate() {
        if session.call().state == CallModelState::Terminated {
            break;
        }
        if n == edited_at {
            session.step_edited(step.clone(), edit);
        } else {
            session.try_step(step.clone());
        }
    }
    Run {
        turns: session.turns().map(|t| format!("{:?} {:?}", t.result, t.notes)).collect(),
        raws: session.texts().to_vec(),
        sent: session.sent(),
        responses: session.responses(),
        defects: session.defects(),
        call: session.call().clone(),
    }
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
