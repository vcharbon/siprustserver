//! What a leg's reactor observed of one message: an in-dialog request it
//! received ([`RequestFact`]) or a response ([`ResponseFact`]), the facts the
//! observed state folds and the barriers read.

use sip_message::header::MediaType;
use sip_message::header::{HeaderName, HeaderValue, SubscriptionState};
use sip_message::sip_str::SipStr;
use sip_message::{SipRequest, SipResponse};
use tokio::time::Instant;

use super::goals::GoalStep;
use super::response::carries_sdp;
use super::runner::ActorState;
use super::state::Observation;

/// One inbound in-dialog request a leg received: what a barrier reads to tell
/// one outcome of an in-dialog exchange from another (a NOTIFY's subscription
/// state and its sipfrag, an INFO's payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestFact {
    pub method: String,
    /// The `Subscription-State` value's state token (RFC 6665 §8.2.3), when
    /// the request carried one.
    pub subscription_state: Option<String>,
    pub body: Vec<u8>,
}

/// One observed inbound response on a leg — status, body presence, and the
/// fork identity (`To`-tag) it carried. `typed` retains the full response while
/// a matcher-carrying reception goal is pending on the leg, or a reception
/// observer is installed, so a content matcher or an observing caller reads
/// headers+body without a second receive.
#[derive(Debug, Clone)]
pub struct ResponseFact {
    pub status: u16,
    pub reason: String,
    /// The CSeq method of the request this response answers — the transaction
    /// it belongs to. A leg carries several concurrent transactions (the INVITE
    /// plus every automatic: PRACK, UPDATE), so a response expectation that
    /// names a method skips the ones that are another transaction's business.
    pub cseq_method: String,
    pub body_len: usize,
    pub body_is_sdp: bool,
    /// The `To`-tag — the early-dialog/fork identity a reception goal's
    /// `early` binding matches (RFC 3261 §12.1.2).
    pub early_tag: Option<String>,
    /// Boxed: retained only for a pending matcher or an installed reception
    /// observer, and kept off the `Observation` fold's common-variant size.
    pub typed: Option<Box<sip_message::SipResponse>>,
}

/// Folds what an in-dialog request said into its leg's observed requests.
pub(super) fn record_request(st: &ActorState<'_>, req: &SipRequest) {
    let subscription_state = req
        .raw(HeaderName::SubscriptionState)
        .next()
        .and_then(|v| SubscriptionState::parse(&SipStr::owned(v)).ok())
        .map(|s| s.token().to_ascii_lowercase());
    st.obs.record(
        Observation::LegRequest {
            leg: st.role,
            call_id: req.call_id().to_string(),
            cseq: req.cseq().seq(),
            fact: RequestFact {
                method: req.method().as_str().to_string(),
                subscription_state,
                body: req.body().to_vec(),
            },
        },
        Instant::now(),
    );
}

/// Fold one inbound response into this leg's ordered response-fact log. The
/// typed message is retained while a matcher-carrying reception goal is still
/// pending on this actor (the content matcher compares it later), and — with a
/// reception observer installed — for every response, so a reception goal that
/// carries no matcher still hands the hook a fully populated message. With
/// neither, nothing is retained.
pub(super) fn record_response_fact(st: &mut ActorState<'_>, resp: &SipResponse, now: Instant) {
    let retain = st.reception_observer.is_some()
        || st
            .goals
            .remaining_steps()
            .any(|s| matches!(s, GoalStep::ExpectResponse { matcher: Some(_), .. }));
    let body_is_sdp = carries_sdp(resp.body(), resp.header::<MediaType>());
    st.obs.record(
        Observation::LegResponse {
            leg: st.role,
            fact: ResponseFact {
                status: resp.status(),
                reason: resp.reason().to_string(),
                cseq_method: resp.cseq().method().as_str().to_string(),
                body_len: resp.body().len(),
                body_is_sdp,
                early_tag: resp.to().tag().map(str::to_string),
                typed: retain.then(|| Box::new(resp.clone())),
            },
        },
        now,
    );
}
