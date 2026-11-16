//! The in-dialog requests a side of the call received, as its facts keep
//! them: what each said, bounded in count and body.

use sip_message::header::{HeaderValue, SubscriptionState};
use sip_message::sip_str::SipStr;
use sip_message::sniff::{body, header_value};

use super::untagged;
use crate::mux::Datagram;

/// One in-dialog request the caller or a callee leg received (ACK and CANCEL
/// aside): what it said, for a reader that tells one outcome of an in-dialog
/// exchange from another (a NOTIFY's subscription state and sipfrag, an
/// INFO's payload).
#[derive(Debug, Clone, PartialEq)]
pub struct LegRequest {
    pub at_ms: i64,
    pub method: String,
    pub(crate) cseq: u32,
    /// The `Subscription-State` value's state token (RFC 6665 §8.2.3).
    pub subscription_state: Option<String>,
    /// The body's first [`LEG_REQUEST_BODY_MAX`] bytes.
    pub body: Vec<u8>,
}

/// The bound on the requests the caller's or a leg's facts keep, and on each
/// one's body.
pub const LEG_REQUESTS_MAX: usize = 8;
pub const LEG_REQUEST_BODY_MAX: usize = 64;

/// Note the in-dialog request `d` received at `now` in `received` (ACK and
/// out-of-dialog requests aside, a retransmission once, the first
/// [`LEG_REQUESTS_MAX`]).
pub(super) fn note_request(received: &mut Vec<LegRequest>, d: &Datagram<'_>, now: i64) {
    let id = d.id();
    let (Some(method), Some(cseq)) = (id.method, id.cseq) else { return };
    let seen = received.iter().any(|r| r.cseq == cseq && r.method == method);
    if method == "ACK" || untagged(id) || seen || received.len() >= LEG_REQUESTS_MAX {
        return;
    }
    let raw = d.raw();
    let subscription_state = header_value(raw, "Subscription-State")
        .and_then(|v| SubscriptionState::parse(&SipStr::owned(&v)).ok())
        .map(|s| s.token().to_ascii_lowercase());
    let body = body(raw).unwrap_or_default();
    received.push(LegRequest {
        at_ms: now,
        method: method.to_string(),
        cseq,
        subscription_state,
        body: body[..body.len().min(LEG_REQUEST_BODY_MAX)].to_vec(),
    });
}
