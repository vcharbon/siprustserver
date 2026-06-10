//! Message **anchors** (ADR-0019): a scenario labels a message it received with
//! a `(agent, anchor)` pair from the canonical anchor vocabulary, and the E2E
//! check engine later resolves `<agent>.<anchor>` to the recorded wire entry.
//!
//! Tagging happens at receive time, where the fluent DSL already holds the
//! parsed message — but the recording is the source of truth, so a tag stores
//! the message's **identity keys** (Call-ID, CSeq, kind, top Via branch) plus
//! the receiving agent's address, and resolution re-finds the matching
//! [`sip_net::RecordedSipEntry`] post-call. Retransmitted copies share the same
//! keys and identical bytes, so "first delivered match" is well-defined.

use std::net::SocketAddr;

use sip_message::{SipMessage, SipRequest, SipResponse};

/// Request/response discriminator of a tagged message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorMsgKind {
    Request { method: String },
    Response { status: u16 },
}

/// The identity keys of a tagged message, extracted from the parsed form the
/// DSL holds at tag time and re-derivable from recorded raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorKeys {
    pub call_id: String,
    pub cseq_seq: u32,
    pub cseq_method: String,
    pub kind: AnchorMsgKind,
    /// Top Via branch — unique per transaction hop, disambiguates same-CSeq
    /// messages on different legs.
    pub via_branch: Option<String>,
}

impl From<&SipRequest> for AnchorKeys {
    fn from(r: &SipRequest) -> Self {
        AnchorKeys {
            call_id: r.call_id.clone(),
            cseq_seq: r.cseq.seq,
            cseq_method: r.cseq.method.to_string(),
            kind: AnchorMsgKind::Request { method: r.method.to_string() },
            via_branch: r.via.first().branch.clone(),
        }
    }
}

impl From<&SipResponse> for AnchorKeys {
    fn from(r: &SipResponse) -> Self {
        AnchorKeys {
            call_id: r.call_id.clone(),
            cseq_seq: r.cseq.seq,
            cseq_method: r.cseq.method.to_string(),
            kind: AnchorMsgKind::Response { status: r.status },
            via_branch: r.via.first().branch.clone(),
        }
    }
}

impl From<&SipMessage> for AnchorKeys {
    fn from(m: &SipMessage) -> Self {
        match m {
            SipMessage::Request(r) => r.into(),
            SipMessage::Response(r) => r.into(),
        }
    }
}

/// One `(agent, anchor)` label, surfaced on the
/// [`RunReport`](crate::run::RunReport).
#[derive(Debug, Clone)]
pub struct AnchorTag {
    /// The tagging agent's logical name (`bob1`).
    pub agent: String,
    /// The canonical anchor name (`initialInvite`).
    pub anchor: String,
    /// The tagging agent's bound address — the recorded entry's receiver.
    pub rx_addr: SocketAddr,
    pub keys: AnchorKeys,
}

impl AnchorTag {
    /// Does a recorded message (re-parsed from raw bytes) match this tag's
    /// identity keys? The caller has already filtered on `to == rx_addr`.
    pub fn matches(&self, msg: &SipMessage) -> bool {
        AnchorKeys::from(msg) == self.keys
    }
}
