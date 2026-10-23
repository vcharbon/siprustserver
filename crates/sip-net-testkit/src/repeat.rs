//! The RFC 3261 §17.2 repeat test: transaction keys plus byte identity.
//!
//! A datagram is a repeat iff it is byte-identical to one already noted under
//! the same transaction key. This is THE one repeat classification in this
//! tree: the recording decorator stamps every arrival with it at event
//! production ([`crate::contracts::WireStamp`]), and the harness absorption
//! seam (`scenario_harness::absorption`) absorbs/surfaces with the same
//! tables — so the audit's wire view and the TU view can never disagree on
//! what a repeat is. What to DO with a repeat (absorb it, keep it, re-ACK) is
//! the caller's policy; this module only answers "have I seen exactly these
//! bytes on this transaction before, and under which token?".

use std::collections::HashMap;
use std::sync::Mutex;

use sip_message::{SipMessage, SipRequest, SipResponse};

/// First-seen bytes per transaction key, remembered under the caller's token
/// for that datagram (the recording seq, an arrival ordinal — any stable id).
#[derive(Default)]
pub struct RepeatTables {
    /// Server-transaction key: Call-ID, top-Via branch, request method.
    /// Call-ID is in the key because a deterministic harness can mint one
    /// branch across two calls, and those are not one transaction; a CANCEL or
    /// ACK sharing its INVITE's branch is its own key (§17.2.3 keys on method).
    requests: Mutex<HashMap<(String, String, String), (u64, Vec<u8>)>>,
    /// Final-response key: Call-ID, branch, CSeq number, CSeq method, status.
    /// Byte-different under one key is a forked 2xx carrying its own To-tag —
    /// a real signal, never a repeat.
    finals: Mutex<HashMap<(String, String, u32, String, u16), (u64, Vec<u8>)>>,
}

impl RepeatTables {
    /// Whether `raw` repeats what its key already holds: `Some(first)` names
    /// the token the first sighting was noted under; `None` remembers `raw`
    /// under `token` as that first sighting. A datagram that keys nothing (no
    /// RFC 3261 magic cookie in the branch) is never a repeat — graceful
    /// degradation to the raw wire behaviour, never a silent drop.
    pub fn note(&self, raw: &[u8], msg: &SipMessage, token: u64) -> Option<u64> {
        match msg {
            SipMessage::Request(r) => {
                let branch = keyable(request_branch(r))?;
                let key = (r.call_id().to_string(), branch, r.method().to_string());
                note(&self.requests, key, raw, token)
            }
            SipMessage::Response(r) if r.status() >= 200 => {
                let branch = keyable(response_branch(r))?;
                let key = (
                    r.call_id().to_string(),
                    branch,
                    r.cseq().seq(),
                    r.cseq().method().to_string(),
                    r.status(),
                );
                note(&self.finals, key, raw, token)
            }
            // Provisionals are NEVER deduped: a second 180 is "ring again", an
            // observable a flow is entitled to see.
            SipMessage::Response(_) => None,
        }
    }
}

/// Whether a repeat of this datagram is the TRANSACTION USER's to re-send, not
/// a transaction layer's to absorb — so it stays in both views:
/// - a 2xx to an INVITE: the client transaction terminated on the first one,
///   and §13.3.1.4 makes the retransmission end-to-end;
/// - an ACK: the ACK to a 2xx is a core-generated dialog message, one per 2xx
///   received. The hop-by-hop ACK of a non-2xx is the exception, and the
///   absorption seam routes it by its armed obligation before ever asking this.
pub fn repeat_belongs_to_tu(msg: &SipMessage) -> bool {
    match msg {
        SipMessage::Request(r) => r.method().as_str() == "ACK",
        SipMessage::Response(r) => {
            (200..300).contains(&r.status()) && r.cseq().method() == "INVITE"
        }
    }
}

/// The `branch` parameter of a request's topmost Via — the transaction key
/// (RFC 3261 §8.1.1.7).
fn request_branch(req: &SipRequest) -> Option<String> {
    req.top_via().branch().map(str::to_string)
}

/// The `branch` of a response's topmost Via — the transaction the response
/// answers (RFC 3261 §17.1.3).
fn response_branch(resp: &SipResponse) -> Option<String> {
    resp.top_via().branch().map(str::to_string)
}

fn keyable(branch: Option<String>) -> Option<String> {
    branch.filter(|b| b.starts_with("z9hG4bK"))
}

fn note<K: std::hash::Hash + Eq>(
    table: &Mutex<HashMap<K, (u64, Vec<u8>)>>,
    key: K,
    raw: &[u8],
    token: u64,
) -> Option<u64> {
    let mut seen = table.lock().unwrap();
    match seen.get(&key) {
        Some((first, bytes)) if bytes.as_slice() == raw => Some(*first),
        // A new key, or the same key with DIFFERENT bytes: not a repeat, and
        // remembered so its OWN repeats are caught.
        _ => {
            seen.insert(key, (token, raw.to_vec()));
            None
        }
    }
}
