//! RFC 3261 §17.2.1 and §8.2.2.2 for this endpoint's INVITE server
//! transactions. A copy of an INVITE the transaction user answered — the same
//! transaction by §17.2.3, its top-Via branch and sent-by, whatever else a
//! proxy stamped on the copy — is absorbed below the transaction user, drawing
//! the latest provisional or non-2xx final sent on its transaction again, and
//! nothing once a 2xx left (RFC 6026 §7.1); a copy before any response is the
//! byte-identity rule's. An INVITE without a To-tag that matches the Call-ID,
//! From-tag and CSeq of one received but not its transaction is a merged
//! request: the UAS core answers it 482 (§8.2.2.2).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::header::SentBy;
use sip_message::{SipRequest, SipResponse};

/// An INVITE server transaction's identity (§17.2.3).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    branch: String,
    sent_by: SentBy,
}

impl Key {
    fn of_request(r: &SipRequest) -> Option<Self> {
        let via = r.top_via();
        Some(Key {
            branch: via.branch().filter(|b| !b.is_empty())?.to_string(),
            sent_by: via.sent_by_ref().to_sent_by(),
        })
    }

    fn of_response(r: &SipResponse) -> Option<Self> {
        let via = r.top_via();
        Some(Key {
            branch: via.branch().filter(|b| !b.is_empty())?.to_string(),
            sent_by: via.sent_by_ref().to_sent_by(),
        })
    }
}

/// The request identity §8.2.2.2 compares: Call-ID, From-tag, CSeq.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Identity {
    call_id: String,
    from_tag: String,
    cseq: u32,
}

impl Identity {
    fn of(r: &SipRequest) -> Self {
        Identity {
            call_id: r.call_id().as_str().to_string(),
            from_tag: r.from().tag().unwrap_or_default().to_string(),
            cseq: r.cseq().seq(),
        }
    }
}

/// A response one INVITE transaction sent, and the first arrival ordinal that
/// came after it: a copy still queued when it left arrived before it.
enum Sent {
    /// A 2xx: a copy draws nothing.
    Accepted { after: u64 },
    /// A provisional or a non-2xx final: a copy draws it again.
    Repeat { wire: Vec<u8>, dst: SocketAddr, after: u64 },
}

impl Sent {
    fn after(&self) -> u64 {
        match self {
            Sent::Accepted { after } | Sent::Repeat { after, .. } => *after,
        }
    }
}

/// What an INVITE arriving at this endpoint is.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Copy {
    /// A copy of one received, its transaction having sent nothing a copy draws.
    Absorbed,
    /// A copy of one received: the response its transaction last sent, and where.
    Repeat(Vec<u8>, SocketAddr),
    /// A merged request: the 482 that answers it, and where (§8.2.2.2, §18.2.2).
    Merged(Vec<u8>, SocketAddr),
}

/// The per-endpoint table of INVITE server transactions.
#[derive(Default)]
pub(super) struct Invited {
    txns: Mutex<HashMap<Key, (Identity, Vec<Sent>)>>,
}

impl Invited {
    /// What `r`, the `ordinal`-th arrival, is: a copy of an INVITE whose
    /// transaction this endpoint holds and answered, or a merged request —
    /// `None` for the first copy of one, which opens the transaction, for a
    /// copy that arrived before any response, and for any other request. A
    /// copy draws the response its transaction had last sent when it arrived,
    /// and nothing when a later one already left.
    pub(super) fn sight(&self, r: &SipRequest, ordinal: u64) -> Option<Copy> {
        if r.method().as_str() != "INVITE" {
            return None;
        }
        let key = Key::of_request(r)?;
        let identity = Identity::of(r);
        let mut txns = self.txns.lock().unwrap();
        if let Some((_, sent)) = txns.get(&key) {
            // The response the copy found sent when it arrived; a later one
            // already answers it, so it draws nothing again.
            let found = sent.iter().rposition(|s| s.after() <= ordinal)?;
            if found + 1 < sent.len() {
                return Some(Copy::Absorbed);
            }
            return match &sent[found] {
                Sent::Accepted { .. } => Some(Copy::Absorbed),
                Sent::Repeat { wire, dst, .. } => Some(Copy::Repeat(wire.clone(), *dst)),
            };
        }
        let merged = r.to().tag().is_none() && txns.values().any(|(id, _)| *id == identity);
        if merged {
            return Some(loop_detected(r).map_or(Copy::Absorbed, |(w, d)| Copy::Merged(w, d)));
        }
        txns.insert(key, (identity, Vec::new()));
        None
    }

    /// Note `resp`, sent as `wire` to `dst` when the datagram arriving next is
    /// the `after`-th, on its INVITE transaction: a later copy of that INVITE
    /// draws it again unless it is a 2xx.
    pub(super) fn note_sent(&self, resp: &SipResponse, wire: &[u8], dst: SocketAddr, after: u64) {
        if !resp.cseq().method().as_str().eq_ignore_ascii_case("INVITE") {
            return;
        }
        let Some(key) = Key::of_response(resp) else { return };
        let mut txns = self.txns.lock().unwrap();
        let Some((_, sent)) = txns.get_mut(&key) else { return };
        sent.push(if (200..300).contains(&resp.status()) {
            Sent::Accepted { after }
        } else {
            Sent::Repeat { wire: wire.to_vec(), dst, after }
        });
    }
}

/// The 482 a merged request draws, toward its top Via's sent-by (§18.2.2).
fn loop_detected(r: &SipRequest) -> Option<(Vec<u8>, SocketAddr)> {
    let resp = generate_response(r, 482, "Loop Detected", &GenerateResponseOpts::default()).ok()?;
    let (host, port) = r.top_via().sent_by().pair();
    let dst = format!("{host}:{port}").parse().ok()?;
    Some((resp.image().to_vec(), dst))
}
