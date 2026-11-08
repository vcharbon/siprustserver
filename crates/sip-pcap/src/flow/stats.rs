//! The ingest counters of a capture, and the messages ingest sets aside.

use std::collections::{BTreeMap, BTreeSet};

use sip_message::error::reason_key;
use sip_message::{sniff, SipMessage, SipParseError};

use super::FlowLeg;
use crate::align::ProbeOffset;
use crate::header_errors;

/// Ingest counters (SIP filter, capture dedup, parse). Decode-level counters
/// live in [`crate::DecodeStats`].
#[derive(Debug, Default, Clone)]
pub struct FlowStats {
    /// Messages ingested into the model.
    pub sip_messages: u64,
    /// Identical (src, dst, payload) datagrams within the capture-dup window —
    /// `-i any` seeing one packet on veth AND bridge, or two probes of one
    /// wire once aligned — not a retransmission.
    pub capture_dups: u64,
    /// SIP-looking datagrams the parser rejected.
    pub parse_failed: u64,
    /// The index in the input slice of each datagram counted in
    /// `parse_failed`, in capture-time order.
    pub parse_failed_at: Vec<usize>,
    /// The `parse_failed` datagrams by the key of their refusal reason
    /// ([`reason_key`]).
    pub parse_failed_by_reason: BTreeMap<String, u64>,
    /// The Call-ID of each `parse_failed` request that opens a dialog or
    /// transaction: no To-tag, neither ACK nor CANCEL.
    pub opening_requests_refused: BTreeSet<String>,
    /// Accepted messages carrying a non-fatal header reading that failed
    /// ([`header_errors`]): ingested like any other, and counted here.
    pub header_failed: u64,
    /// The index in the input slice of each datagram counted in
    /// `header_failed`, in capture-time order.
    pub header_failed_at: Vec<usize>,
    /// The `header_failed` messages by the key of each failed reading's
    /// reason: a message counts once under each key its readings name.
    pub header_failed_by_reason: BTreeMap<String, u64>,
    /// Every failed reading of those messages, in capture-time order.
    pub header_failures: Vec<HeaderFailure>,
    /// Datagrams that did not look like SIP (RTP/STUN/DNS on captured ports).
    pub non_sip: u64,
    /// Every stretch of every probe rebased onto another probe's clock before
    /// the dedup ([`crate::align`]), a zero stretch included, with the copies
    /// now counted in `capture_dups`.
    pub aligned_probes: Vec<ProbeOffset>,
}

/// One failed non-fatal header reading of an ingested message: the message
/// (its leg's Call-ID and its position in that leg), the header and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderFailure {
    pub call_id: String,
    pub msg: usize,
    pub header: String,
    pub reason: String,
}

impl FlowStats {
    /// Count datagram `at` of the input, `raw`, as refused by the parser for
    /// `error`. A refused request that opens a dialog or transaction (no
    /// To-tag, RFC 3261 §8.1.1.2; not an ACK or CANCEL, which belong to the
    /// INVITE's) has its Call-ID kept, read off its identity lines.
    pub(crate) fn note_refusal(&mut self, at: usize, error: &SipParseError, raw: &[u8]) {
        self.parse_failed += 1;
        self.parse_failed_at.push(at);
        *self.parse_failed_by_reason.entry(error.reason_key()).or_default() += 1;
        let id = sniff::identity(raw);
        let opens = id
            .method
            .is_some_and(|m| !m.eq_ignore_ascii_case("ACK") && !m.eq_ignore_ascii_case("CANCEL"));
        if let (true, None, Some(call_id)) =
            (opens, id.to_tag.filter(|t| !t.is_empty()), id.call_id)
        {
            self.opening_requests_refused.insert(call_id.to_string());
        }
    }

    /// Mark every leg whose Call-ID names a refused opening request.
    pub(crate) fn mark_opening_requests_set_aside(&self, legs: &mut [FlowLeg]) {
        for leg in legs {
            leg.opening_request_set_aside = self.opening_requests_refused.contains(&leg.call_id);
        }
    }

    /// Count `msg`, datagram `at` of the input, and record its failed header
    /// readings, when it carries any; it is the `msg`-th message of the leg
    /// named `call_id`.
    pub(crate) fn note_header_failures(
        &mut self,
        at: usize,
        msg: &SipMessage,
        call_id: &str,
        position: usize,
    ) {
        let failed = header_errors::of(msg);
        if failed.is_empty() {
            return;
        }
        self.header_failed += 1;
        self.header_failed_at.push(at);
        let keys: BTreeSet<String> = failed.iter().map(|e| reason_key(&e.reason)).collect();
        for key in keys {
            *self.header_failed_by_reason.entry(key).or_default() += 1;
        }
        self.header_failures.extend(failed.into_iter().map(|e| HeaderFailure {
            call_id: call_id.to_string(),
            msg: position,
            header: e.name.as_wire_str().to_string(),
            reason: e.reason,
        }));
    }

    /// The datagrams ingest sets aside: the ones the parser refused and the
    /// accepted ones carrying a failed header reading, as input indexes.
    pub fn set_aside_at(&self) -> impl Iterator<Item = usize> + '_ {
        self.parse_failed_at.iter().chain(&self.header_failed_at).copied()
    }
}
