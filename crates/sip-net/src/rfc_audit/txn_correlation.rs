//! Port of `tests/harness/rules/rfc/_transaction-correlation.ts` — a top-Via
//! `branch=` keyed index of the requests/responses on each client transaction,
//! reused by the CANCEL/ACK/PRACK-vs-INVITE cross rules.
//!
//! The transaction key in RFC 3261 §17 is the **top-Via branch** (`z9hG4bK…`)
//! combined with the request method; for the rules in this family the branch
//! alone is sufficient because each agent's ordered event stream is already
//! partitioned per Call-ID by the projector — a collision would imply a
//! branch-uniqueness violation already surfaced by `rfc.branchPrefix` /
//! `rfc.via`.
//!
//! [`build_branch_index`] is a single one-pass index over a recorded
//! `&[Stamped<SignalingNetworkEvent>]` slice that buckets sent/received ×
//! request/response by top-Via branch. Rule writers then ask narrow questions:
//! "what was the sent INVITE on branch X?", "did any final response arrive on
//! branch X?", "what is the ordered list of responses on branch X?".
//!
//! Eight planned RFC 3261 cross-message rules consume this helper:
//! `rfc.ackRequireSubsetOfInvite`, `rfc.cancelRouteEchoesInvite`,
//! `rfc.cancelAfter1xx`, `rfc.serialRegister`,
//! `rfc.noReInviteWhileInviteInProgress`, `rfc.proxy100WithinT100ms`,
//! `rfc.strictRouteRewriteHandled`, `rfc.ackPreservesInviteRoute`.
//!
//! Pure / deterministic — no clocks, no randomness. Mirrors the TS field names
//! in Rust snake_case (`sent_requests` / `received_requests` /
//! `sent_responses` / `received_responses`, keyed `by_branch`). Each stored
//! message keeps its parsed [`SipMessage`] plus the originating `bind_key` and
//! the send/receive [`Direction`], so the consuming rules can read every field
//! they compare (method, CSeq, Request-URI, Route / Record-Route set, the full
//! header list) off the same entry.

use std::collections::HashMap;

use layer_harness::{LaneKey, Stamped};
use sip_message::message_helpers::get_headers;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest, SipResponse};

use crate::contracts::SignalingNetworkEvent;
use crate::rfc_audit::dialog_model::{cseq_method, cseq_seq, msg_headers, top_via_branch};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Which side of the wire an indexed message was observed on. Mirrors the TS
/// `AgentDirection = "sent" | "received"` — `Sent` is a `SendCalled`, `Received`
/// is a `RecvItem`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Sent,
    Received,
}

/// One indexed message on a transaction branch, retaining the originating lane
/// and direction alongside the parsed message so the cross rules can read any
/// field (Request-URI, Route set, headers, CSeq, method/status) off it.
#[derive(Clone, Debug)]
pub struct IndexedMessage {
    /// The lane the message was sent/received on (`SendCalled`/`RecvItem`
    /// `bind_key`).
    pub bind_key: LaneKey,
    /// Send vs receive — the TS `kind`.
    pub direction: Direction,
    /// The fully parsed message; its `Request`/`Response` arm tells request from
    /// response.
    pub msg: SipMessage,
}

/// All four directional buckets for a single top-Via branch — the TS
/// `BranchEntry`. Each list is in event (insertion) order.
#[derive(Clone, Debug, Default)]
pub struct BranchEntry {
    /// Sent requests on this branch, in insertion order.
    pub sent_requests: Vec<IndexedMessage>,
    /// Received requests on this branch, in insertion order.
    pub received_requests: Vec<IndexedMessage>,
    /// Sent responses on this branch, in insertion order.
    pub sent_responses: Vec<IndexedMessage>,
    /// Received responses on this branch, in insertion order.
    pub received_responses: Vec<IndexedMessage>,
}

/// Per-branch index built from a recorded event stream — the TS `BranchIndex`.
/// Lookups are by the raw branch string (including the `z9hG4bK` prefix).
#[derive(Clone, Debug, Default)]
pub struct BranchIndex {
    pub by_branch: HashMap<String, BranchEntry>,
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

/// One-pass build of the per-branch index over a recorded
/// `&[Stamped<SignalingNetworkEvent>]`. Only `SendCalled` (→ [`Direction::Sent`])
/// and `RecvItem` (→ [`Direction::Received`]) carry a message; the lifecycle
/// variants are ignored. Events whose payload fails to parse, or that lack a
/// non-empty top-Via branch, are skipped silently — `rfc.via` /
/// `rfc.branchPrefix` own the "missing branch" obligation, so this helper does
/// not double-report.
pub fn build_branch_index(events: &[Stamped<SignalingNetworkEvent>]) -> BranchIndex {
    let parser = CustomParser::new();
    let mut by_branch: HashMap<String, BranchEntry> = HashMap::new();

    for s in events {
        let (bind_key, direction, raw) = match &s.event {
            SignalingNetworkEvent::SendCalled { bind_key, msg, .. } => {
                (bind_key, Direction::Sent, msg.as_slice())
            }
            SignalingNetworkEvent::RecvItem { bind_key, packet, .. } => {
                (bind_key, Direction::Received, packet.raw.as_slice())
            }
            _ => continue,
        };
        let Ok(msg) = parser.parse(raw) else {
            continue;
        };
        let Some(branch) = top_via_branch(&msg) else {
            continue;
        };

        let entry = by_branch.entry(branch).or_default();
        let indexed = IndexedMessage {
            bind_key: bind_key.clone(),
            direction,
            msg,
        };
        let bucket = match (&indexed.msg, direction) {
            (SipMessage::Request(_), Direction::Sent) => &mut entry.sent_requests,
            (SipMessage::Request(_), Direction::Received) => &mut entry.received_requests,
            (SipMessage::Response(_), Direction::Sent) => &mut entry.sent_responses,
            (SipMessage::Response(_), Direction::Received) => &mut entry.received_responses,
        };
        bucket.push(indexed);
    }

    BranchIndex { by_branch }
}

// ---------------------------------------------------------------------------
// Accessors
// ---------------------------------------------------------------------------

impl BranchEntry {
    /// The requests on this branch in the given direction.
    fn requests(&self, dir: Direction) -> &[IndexedMessage] {
        match dir {
            Direction::Sent => &self.sent_requests,
            Direction::Received => &self.received_requests,
        }
    }

    /// The responses on this branch in the given direction.
    fn responses(&self, dir: Direction) -> &[IndexedMessage] {
        match dir {
            Direction::Sent => &self.sent_responses,
            Direction::Received => &self.received_responses,
        }
    }
}

impl IndexedMessage {
    /// Borrow the message as a request, or `None` if it is a response.
    pub fn as_request(&self) -> Option<&SipRequest> {
        match &self.msg {
            SipMessage::Request(r) => Some(r),
            SipMessage::Response(_) => None,
        }
    }

    /// Borrow the message as a response, or `None` if it is a request.
    pub fn as_response(&self) -> Option<&SipResponse> {
        match &self.msg {
            SipMessage::Response(r) => Some(r),
            SipMessage::Request(_) => None,
        }
    }
}

impl BranchIndex {
    /// Ordered slice of all responses on `branch` in the given direction; empty
    /// when the branch is unknown. Mirrors the TS `responsesFor`.
    pub fn responses_for(&self, branch: &str, dir: Direction) -> &[IndexedMessage] {
        self.by_branch
            .get(branch)
            .map(|e| e.responses(dir))
            .unwrap_or(&[])
    }

    /// Ordered slice of all requests on `branch` in the given direction; empty
    /// when the branch is unknown.
    pub fn requests_for(&self, branch: &str, dir: Direction) -> &[IndexedMessage] {
        self.by_branch
            .get(branch)
            .map(|e| e.requests(dir))
            .unwrap_or(&[])
    }

    /// The first request whose CSeq method matches `method` (case-insensitive)
    /// on `branch` in the given direction, or `None`. Mirrors the TS
    /// `findRequestByBranch`.
    ///
    /// Branch alone identifies a client transaction per RFC 3261 §17, but a
    /// branch may carry both an INVITE and its later ACK in a retransmit-free
    /// flow; restricting by method picks the intended one.
    pub fn find_request_by_branch(
        &self,
        branch: &str,
        method: &str,
        dir: Direction,
    ) -> Option<&IndexedMessage> {
        self.requests_for(branch, dir)
            .iter()
            .find(|m| cseq_method(&m.msg).eq_ignore_ascii_case(method))
    }

    /// Convenience for the common INVITE lookup. Mirrors the TS
    /// `findInviteByBranch`; defaults to the *sent* side because most rules pair
    /// "ACK we sent" with "INVITE we sent".
    pub fn find_invite_by_branch(&self, branch: &str, dir: Direction) -> Option<&IndexedMessage> {
        self.find_request_by_branch(branch, "INVITE", dir)
    }

    /// True iff any final (status ≥ 200) response appears on `branch` in the
    /// given direction. Mirrors the TS `hasFinalResponseFor`.
    pub fn has_final_response_for(&self, branch: &str, dir: Direction) -> bool {
        self.responses_for(branch, dir)
            .iter()
            .any(|m| status_of(&m.msg) >= 200)
    }

    /// The first response status seen on `branch` (1xx included) in the given
    /// direction, or `None` when no response was observed. Mirrors the TS
    /// `firstResponseStatusFor`.
    pub fn first_response_status_for(&self, branch: &str, dir: Direction) -> Option<u16> {
        self.responses_for(branch, dir)
            .first()
            .map(|m| status_of(&m.msg))
    }
}

/// Response status, or `0` for a request — the responses buckets only ever hold
/// responses, so this is total over what we feed it.
fn status_of(m: &SipMessage) -> u16 {
    match m {
        SipMessage::Response(r) => r.status,
        SipMessage::Request(_) => 0,
    }
}

// ---------------------------------------------------------------------------
// Header utilities — option-tag parsing shared with the cross-message rules.
// ---------------------------------------------------------------------------

/// Split comma-separated option-tag header values (Require / Supported /
/// Proxy-Require / Unsupported) into normalised lower-case tags. Empty pieces
/// are dropped. Mirrors the TS `splitOptionTags`.
pub fn split_option_tags<I, S>(values: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = Vec::new();
    for v in values {
        for piece in v.as_ref().split(',') {
            let tag = piece.trim().to_ascii_lowercase();
            if !tag.is_empty() {
                out.push(tag);
            }
        }
    }
    out
}

/// All values of a (possibly repeated) header on the indexed message — a thin
/// wrapper over [`get_headers`] for rules that compare Route / Record-Route /
/// Require lists pulled straight off an [`IndexedMessage`].
pub fn header_values<'a>(m: &'a IndexedMessage, name: &str) -> Vec<&'a str> {
    get_headers(msg_headers(&m.msg), name)
}

/// CSeq `(number, method)` of an indexed message — re-exported convenience so a
/// rule comparing an ACK/CANCEL to its INVITE doesn't re-match the arms.
pub fn cseq_pair(m: &IndexedMessage) -> (u32, String) {
    (cseq_seq(&m.msg), cseq_method(&m.msg).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::UdpPacket;

    /// A minimal, parseable request with caller-controlled method, CSeq and
    /// top-Via `branch`.
    fn req(method: &str, cseq: u32, branch: &str) -> Vec<u8> {
        format!(
            "{method} sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5091;branch={branch}\r\n\
             From: <sip:b2bua@127.0.0.1>;tag=ft\r\n\
             To: <sip:bob@127.0.0.1>;tag=btag\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// A minimal parseable response carrying a chosen `(status, CSeq, method)`
    /// and a top-Via `branch`.
    fn resp(status: u16, cseq: u32, method: &str, branch: &str) -> Vec<u8> {
        format!(
            "SIP/2.0 {status} OK\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5091;branch={branch}\r\n\
             From: <sip:b2bua@127.0.0.1>;tag=ft\r\n\
             To: <sip:bob@127.0.0.1>;tag=btag\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: {cseq} {method}\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// Wrap raw bytes as a `RecvItem` at `bind` (received side).
    fn recv_at(bind: &str, raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::RecvItem {
                bind_key: bind.to_string(),
                disposition: crate::types::RecvDisposition::Delivered,
                packet: UdpPacket {
                    raw,
                    src: "127.0.0.1:5091".parse().unwrap(),
                    arrival_ms: seq,
                },
            },
            seq,
            at_ms: seq,
        }
    }

    /// Wrap raw bytes as a `SendCalled` at `bind` (sent side).
    fn send_at(bind: &str, raw: Vec<u8>, seq: u64) -> Stamped<SignalingNetworkEvent> {
        Stamped {
            event: SignalingNetworkEvent::SendCalled {
                bind_key: bind.to_string(),
                to: "127.0.0.1:5070".parse().unwrap(),
                msg: raw,
            },
            seq,
            at_ms: seq,
        }
    }

    #[test]
    fn cancel_correlates_to_its_invite_branch() {
        // A CANCEL reuses the branch of the INVITE it cancels (RFC 3261 §9.1):
        // the index buckets both under the same branch, so the rule can pair
        // them.
        let branch = "z9hG4bK-inv";
        let evs = vec![
            send_at("b2bua", req("INVITE", 1, branch), 0),
            send_at("b2bua", req("CANCEL", 1, branch), 1),
        ];
        let idx = build_branch_index(&evs);

        let invite = idx
            .find_invite_by_branch(branch, Direction::Sent)
            .expect("INVITE bucketed under its branch");
        assert_eq!(cseq_method(&invite.msg), "INVITE");

        let cancel = idx
            .find_request_by_branch(branch, "CANCEL", Direction::Sent)
            .expect("CANCEL correlated to the same branch");
        assert_eq!(cseq_pair(cancel), (1, "CANCEL".to_string()));
        assert_eq!(cancel.bind_key, "b2bua");
        // Both requests share the one branch entry.
        assert_eq!(idx.requests_for(branch, Direction::Sent).len(), 2);
    }

    #[test]
    fn ack_correlates_to_its_invite_branch() {
        // ACK for a non-2xx final reuses the INVITE's branch (RFC 3261 §17.1.1.3);
        // both land in the same branch bucket on the received side.
        let branch = "z9hG4bK-inv2";
        let evs = vec![
            recv_at("bob", req("INVITE", 7, branch), 0),
            recv_at("bob", resp(486, 7, "INVITE", branch), 1),
            recv_at("bob", req("ACK", 7, branch), 2),
        ];
        let idx = build_branch_index(&evs);

        assert!(
            idx.find_invite_by_branch(branch, Direction::Received)
                .is_some(),
            "INVITE present on the branch",
        );
        let ack = idx
            .find_request_by_branch(branch, "ACK", Direction::Received)
            .expect("ACK correlated to the INVITE branch");
        assert_eq!(cseq_pair(ack), (7, "ACK".to_string()));
        // The 486 final is visible on the same branch.
        assert!(idx.has_final_response_for(branch, Direction::Received));
        assert_eq!(
            idx.first_response_status_for(branch, Direction::Received),
            Some(486),
        );
    }

    #[test]
    fn unseen_branch_is_absent() {
        let evs = vec![send_at("b2bua", req("INVITE", 1, "z9hG4bK-known"), 0)];
        let idx = build_branch_index(&evs);

        assert!(!idx.by_branch.contains_key("z9hG4bK-missing"));
        assert!(idx
            .find_invite_by_branch("z9hG4bK-missing", Direction::Sent)
            .is_none());
        assert!(idx
            .responses_for("z9hG4bK-missing", Direction::Received)
            .is_empty());
        assert!(!idx.has_final_response_for("z9hG4bK-missing", Direction::Sent));
        assert_eq!(
            idx.first_response_status_for("z9hG4bK-missing", Direction::Sent),
            None,
        );
    }

    #[test]
    fn sent_and_received_directions_are_distinct_buckets() {
        // The same branch observed once sent and once received splits across the
        // two directional buckets.
        let branch = "z9hG4bK-dir";
        let evs = vec![
            send_at("b2bua", req("OPTIONS", 4, branch), 0),
            recv_at("b2bua", resp(200, 4, "OPTIONS", branch), 1),
        ];
        let idx = build_branch_index(&evs);

        assert_eq!(idx.requests_for(branch, Direction::Sent).len(), 1);
        assert_eq!(idx.requests_for(branch, Direction::Received).len(), 0);
        assert_eq!(idx.responses_for(branch, Direction::Received).len(), 1);
        assert_eq!(idx.responses_for(branch, Direction::Sent).len(), 0);
    }

    #[test]
    fn split_option_tags_normalises_and_drops_empty() {
        let got = split_option_tags(["100rel, Timer", "  ", "Replaces"]);
        assert_eq!(got, vec!["100rel", "timer", "replaces"]);
    }

    #[test]
    fn unparseable_and_branchless_events_are_skipped() {
        // Garbage bytes and a request with no Via branch both drop out silently.
        let no_branch = b"INFO sip:bob@127.0.0.1 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5091\r\n\
             From: <sip:a@h>;tag=ft\r\n\
             To: <sip:b@h>;tag=tt\r\n\
             Call-ID: cid-1@127.0.0.1\r\n\
             CSeq: 1 INFO\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n"
            .to_vec();
        let evs = vec![
            recv_at("bob", b"not a sip message".to_vec(), 0),
            recv_at("bob", no_branch, 1),
        ];
        let idx = build_branch_index(&evs);
        assert!(idx.by_branch.is_empty());
    }
}
