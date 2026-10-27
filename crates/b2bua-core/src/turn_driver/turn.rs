//! What one driven step came to ([`Outcome`]) and, for a turn, what it
//! decided and sent ([`Turn`]).

use call::helpers::Resolved;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest, SipResponse};

use crate::dialog_gate::Verdict;
use crate::effects::{HandlerResult, OutboundBody};
use crate::turn_notes::TurnNotes;

/// What a step came to.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// The event took a turn.
    Turned(Box<Turn>),
    /// The dialog gate kept the event from the rules: answered (481, 500,
    /// 200 or 487, [`Verdict::status`]) or dropped.
    Gated(Verdict),
    /// The timer screen refused the fire: its instance is not the ledger's.
    Screened,
    /// The step's SIP text, as edited, does not parse: the shell drops it.
    Unparsed,
    /// The call gives the step nothing to say, or ended before it.
    Nothing,
    /// The clock moved on.
    Advanced,
}

impl Outcome {
    /// The turn the step took, if it took one.
    pub fn turn(&self) -> Option<&Turn> {
        match self {
            Outcome::Turned(turn) => Some(turn),
            _ => None,
        }
    }

    /// The status the dialog gate answered the step's request with, if it
    /// answered it.
    pub fn gate_status(&self) -> Option<u16> {
        match self {
            Outcome::Gated(verdict) => verdict.status(),
            _ => None,
        }
    }
}

/// One turn: its landed result, what it noted, the rule that handled it and
/// what the gate resolved its message to.
#[derive(Clone, Debug)]
pub struct Turn {
    /// The turn's result as it lands: the record and every effect.
    pub result: HandlerResult,
    /// What the turn noted.
    pub notes: TurnNotes,
    /// The rule that handled the event, by its id; `None` when no rule did.
    pub rule: Option<&'static str>,
    /// What the dialog gate resolved the event's message to.
    pub resolved: Resolved,
}

impl Turn {
    /// The requests the turn sends on `leg`, as composed or as the bytes they
    /// left as, in order.
    pub fn requests_to(&self, leg: &str) -> Vec<SipRequest> {
        self.outbound_to(leg)
            .filter_map(|m| match m {
                SipMessage::Request(r) => Some(r),
                SipMessage::Response(_) => None,
            })
            .collect()
    }

    /// The method and CSeq number of each request the turn sends on `leg`.
    pub fn methods_to(&self, leg: &str) -> Vec<(String, u32)> {
        let named = |r: SipRequest| (r.method().as_str().to_string(), r.cseq().seq());
        self.requests_to(leg).into_iter().map(named).collect()
    }

    /// The first request of `method` the turn sends on `leg`.
    pub fn request_to(&self, leg: &str, method: &str) -> Option<SipRequest> {
        self.requests_to(leg).into_iter().find(|r| r.method().as_str() == method)
    }

    /// The responses the turn sends on `leg`, in order.
    pub fn responses_to(&self, leg: &str) -> Vec<SipResponse> {
        self.outbound_to(leg)
            .filter_map(|m| match m {
                SipMessage::Response(r) => Some(r),
                SipMessage::Request(_) => None,
            })
            .collect()
    }

    /// The status of each response the turn sends on `leg`.
    pub fn statuses_to(&self, leg: &str) -> Vec<u16> {
        self.responses_to(leg).iter().map(SipResponse::status).collect()
    }

    /// The bytes of each message the turn sends on `leg`.
    pub fn wire_to(&self, leg: &str) -> Vec<Vec<u8>> {
        self.result
            .effects
            .outbound
            .iter()
            .filter(|e| e.leg_id.as_deref() == Some(leg))
            .map(|e| match &e.body {
                OutboundBody::Request(r) => r.image().to_vec(),
                OutboundBody::Response(r) => r.image().to_vec(),
                OutboundBody::Datagram(d) => d.wire().0.to_vec(),
            })
            .collect()
    }

    /// The messages the turn sends on `leg`, a datagram read back.
    fn outbound_to<'a>(&'a self, leg: &'a str) -> impl Iterator<Item = SipMessage> + 'a {
        self.result
            .effects
            .outbound
            .iter()
            .filter(move |e| e.leg_id.as_deref() == Some(leg))
            .filter_map(|e| match &e.body {
                OutboundBody::Request(r) => Some(SipMessage::Request(r.clone())),
                OutboundBody::Response(r) => Some(SipMessage::Response(r.clone())),
                OutboundBody::Datagram(d) => CustomParser::new().parse(d.wire().0).ok(),
            })
    }
}
