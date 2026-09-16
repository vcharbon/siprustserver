//! The wire model every rule reads: one observed message stream, the
//! observation that bounds it, and nothing adapter-specific.
//!
//! A [`WireView`] is ONE vantage's stream in capture order — a capture leg
//! (both peers' traffic across its hops) or a live bind's sent+received
//! events. A rule reasons per ENDPOINT within it, never per stream as a
//! whole: a message with `dst == E` is one E took, a message with `src == E`
//! is one E emitted, and the order of those facts is the evidence.
//!
//! **Repeats are marked, not removed.** A message flagged [`Msg::repeat`] does
//! not open an obligation (retransmitting until answered is required
//! behaviour), but it can MEET one: an ACK that is a repeat is still an ACK on
//! the wire. Each rule states which reading it applies. The mark is bounded by
//! the transaction envelope the adapter applies (64·T1), so a re-emission past
//! it arrives unmarked and opens its own obligation.

use std::collections::BTreeMap;
use std::net::SocketAddr;

/// `ip:port`, plus an optional `#label` for a logical endpoint sharing a
/// socket. Compared as an opaque token; [`endpoint_addr`] is the one read
/// inside it.
pub type Endpoint = String;

/// The socket address an [`Endpoint`] names, its `#label` left off — the read
/// for a rule that must know whether a URI a message carries names the
/// endpoint that emitted it. `None` where the token is not an `ip:port`.
pub fn endpoint_addr(endpoint: &str) -> Option<SocketAddr> {
    endpoint.split('#').next()?.parse().ok()
}

/// What kind of SIP message crossed, with the half of the start line a rule
/// keys on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Request { method: String },
    Response { status: u16 },
}

/// One observed message, reduced to the facts every obligation rule keys on.
/// Header reads beyond these go through [`Msg::head`] and `sip_message`
/// accessors — a rule never parses wire syntax itself.
#[derive(Debug, Clone)]
pub struct Msg {
    /// Observation timestamp, microseconds (capture clock or harness clock).
    pub at_us: u64,
    pub src: Endpoint,
    pub dst: Endpoint,
    /// The capture hop the message crossed (0 where the vantage has one wire).
    pub hop: usize,
    /// A repeat of an earlier sighting under its transaction key, WITHIN that
    /// transaction's envelope — the canonical mark, filled by the adapter
    /// (capture: `repeat_of`; live: the recording decorator's §17.2 wire
    /// stamp). Matching bytes emitted after the envelope are a fresh event and
    /// arrive here unmarked.
    pub repeat: bool,
    pub kind: Kind,
    /// The call this message belongs to (RFC 3261 §8.1.1.4). A view can carry
    /// MANY calls — a live bind serves every dialog on its socket — so a rule
    /// keying a transaction needs this alongside the CSeq number, which is only
    /// unique within one call.
    pub call_id: String,
    /// CSeq sequence number and method — present on every readable message.
    pub cseq: u32,
    pub cseq_method: String,
    /// The top-Via branch, when the vantage carried it (RFC 3261 §17 names a
    /// transaction by it). `None` where the message had none or the vantage
    /// does not record one: a rule that needs it returns `Undecidable` for that
    /// occasion, never a guess.
    pub via_branch: Option<String>,
    pub from_tag: Option<String>,
    pub to_tag: Option<String>,
    /// The header block as bytes, for rule-specific reads (`RSeq`, `RAck`,
    /// `100rel` offers) via `sip_message::sniff`. `None` where even the head
    /// is unreadable — such a message can key nothing.
    pub head: Option<Vec<u8>>,
    /// The BODY bytes, for a rule that reads what a message CARRIED (the SDP
    /// offer/answer family). `None` where the vantage did not carry them — a
    /// rule needing a body fact from a body-less vantage returns `Undecidable`,
    /// never a guess. `Some(&[])` is the opposite fact: the message
    /// demonstrably carried no body.
    pub body: Option<Vec<u8>>,
}

impl Msg {
    pub fn is_request(&self, method: &str) -> bool {
        matches!(&self.kind, Kind::Request { method: m } if m.eq_ignore_ascii_case(method))
    }

    /// The response status, or `None` on a request.
    pub fn status(&self) -> Option<u16> {
        match &self.kind {
            Kind::Response { status } => Some(*status),
            Kind::Request { .. } => None,
        }
    }

    /// The session description this message DECLARED it carried: the body under
    /// `application/sdp`, the SDP part of a `multipart/…` body (RFC 5621 §3.1),
    /// `None` where the head or body is unreadable, empty, or names neither.
    pub fn sdp(&self) -> Option<&[u8]> {
        let head = self.head.as_deref()?;
        let body = self.body.as_deref()?;
        use sip_message::header::{HeaderValue, MediaType};
        let ct = sip_message::sniff::header_value(head, "Content-Type")?;
        let ct = MediaType::parse(&sip_message::SipStr::owned(&ct)).ok()?;
        sip_message::sdp_range(&ct, body).map(|r| &body[r])
    }
}

/// How long the OBSERVATION ran, and whether its end is the end of evidence.
///
/// A rule whose offence is an absence needs this: an absence is only chargeable
/// while something was still watching. The two adapters fill it oppositely —
/// the capture supplies the recording span and keeps the rules' observability
/// windows; the live harness controls shutdown, so it supplies `closed: true`
/// and an absence at end-of-stream decides immediately (windows collapse).
#[derive(Debug, Clone, Default)]
pub struct Observation {
    /// The last timestamp anywhere in the observation (a capture's whole
    /// document, not just this view's stream: a leg can fall silent while the
    /// recording goes on watching every other wire).
    pub last_us: u64,
    /// Per endpoint: the last timestamp the observation carries that
    /// endpoint's own traffic — evidence about the vantage, never a gate.
    pub endpoint_last_us: BTreeMap<Endpoint, u64>,
    /// End-of-stream is end-of-world: nothing was left in flight when the
    /// observation stopped, so absence needs no window.
    pub closed: bool,
}

impl Observation {
    /// The last time the observation carried `endpoint`'s own traffic, or 0
    /// where it never names it.
    pub fn last_seen(&self, endpoint: &str) -> u64 {
        self.endpoint_last_us.get(endpoint).copied().unwrap_or(0)
    }

    /// Whether an absence following `at_us` is decidable: the observation
    /// demonstrably ran `window_us` past it, or it is closed.
    pub fn absence_decidable(&self, at_us: u64, window_us: u64) -> bool {
        self.closed || self.last_us.saturating_sub(at_us) >= window_us
    }
}

/// What a rule is handed: one vantage's stream and the observation around it.
pub struct WireView<'a> {
    /// The stream in observation order. A finding's anchor indexes into it.
    pub msgs: &'a [Msg],
    pub obs: &'a Observation,
}

#[cfg(test)]
mod tests {
    use super::endpoint_addr;

    #[test]
    fn endpoint_addr_reads_the_socket_address_under_a_label() {
        assert_eq!(endpoint_addr("10.0.0.9:5080").map(|a| a.port()), Some(5080));
        assert_eq!(endpoint_addr("10.0.0.9:5080#proxy"), endpoint_addr("10.0.0.9:5080"));
        assert_eq!(
            endpoint_addr("[::1]:5080").map(|a| a.ip().to_string()),
            Some("::1".to_string())
        );
        assert_eq!(endpoint_addr("proxy.example:5080"), None, "a name is not an address");
    }
}
