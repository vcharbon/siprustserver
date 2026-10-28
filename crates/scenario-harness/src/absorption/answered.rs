//! RFC 3261 §17.2.2 for this endpoint's non-INVITE server transactions. A
//! copy of a request the transaction user received — the same transaction by
//! §17.2.3, its top-Via branch, sent-by and method, whatever else a proxy
//! stamped on the copy — is absorbed below the transaction user: with nothing
//! sent while no final has left (Trying, Proceeding), with that final again,
//! byte for byte, once one has, until Timer J (64·T1 over UDP). A copy is
//! judged at its arrival, however late it is read: one still queued when the
//! final left arrived before it, one arrived inside Timer J draws the final.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;

use sip_message::header::SentBy;
use sip_message::{SipRequest, SipResponse};
use tokio::time::Instant;

/// Timer J over UDP: 64·T1 (RFC 3261 §17.2.2).
const TIMER_J: Duration = Duration::from_millis(64 * 500);

/// A non-INVITE server transaction's identity (§17.2.3).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    branch: String,
    sent_by: SentBy,
    method: String,
}

impl Key {
    fn of_request(r: &SipRequest) -> Option<Self> {
        let via = r.top_via();
        Some(Key {
            branch: via.branch().filter(|b| !b.is_empty())?.to_string(),
            sent_by: via.sent_by_ref().to_sent_by(),
            method: r.method().as_str().to_ascii_uppercase(),
        })
    }

    fn of_response(r: &SipResponse) -> Option<Self> {
        let via = r.top_via();
        Some(Key {
            branch: via.branch().filter(|b| !b.is_empty())?.to_string(),
            sent_by: via.sent_by_ref().to_sent_by(),
            method: r.cseq().method().as_str().to_ascii_uppercase(),
        })
    }
}

/// Where a server transaction stands.
enum State {
    /// The request reached the transaction user, which sent no final yet.
    Open,
    /// A final left: the one it repeats, where, until when, and the first
    /// arrival ordinal that came after it.
    Completed { wire: Vec<u8>, dst: SocketAddr, until: Instant, after: u64 },
}

/// What a copy of a request draws.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Copy {
    /// Nothing: its transaction has not sent its final to it.
    Absorbed,
    /// The final its transaction sent, and where.
    Repeat(Vec<u8>, SocketAddr),
}

/// The per-endpoint table of non-INVITE server transactions.
#[derive(Default)]
pub(super) struct Answered {
    txns: Mutex<HashMap<Key, State>>,
}

impl Answered {
    /// What `r`, the `ordinal`-th arrival, arrived at `arrived`, is: a copy
    /// of a request whose transaction this endpoint holds — `None` for the
    /// first copy of one, which opens the transaction, and for any request no
    /// transaction takes (an INVITE, an ACK, one arrived past Timer J).
    pub(super) fn sight(&self, r: &SipRequest, ordinal: u64, arrived: Instant) -> Option<Copy> {
        if matches!(r.method().as_str(), "INVITE" | "ACK") {
            return None;
        }
        let key = Key::of_request(r)?;
        let mut txns = self.txns.lock().unwrap();
        match txns.get(&key) {
            None => {
                txns.insert(key, State::Open);
                None
            }
            Some(State::Open) => Some(Copy::Absorbed),
            Some(State::Completed { until, .. }) if arrived >= *until => {
                txns.insert(key, State::Open);
                None
            }
            Some(State::Completed { after, .. }) if ordinal < *after => Some(Copy::Absorbed),
            Some(State::Completed { wire, dst, .. }) => Some(Copy::Repeat(wire.clone(), *dst)),
        }
    }

    /// Note `resp`, sent as `wire` to `dst` when the datagram arriving next is
    /// the `after`-th: a final to a non-INVITE request completes its server
    /// transaction for Timer J; a repeat of it keeps the transaction it
    /// completed, Timer J running from the first. Anything else is not noted.
    pub(super) fn note_sent(&self, resp: &SipResponse, wire: &[u8], dst: SocketAddr, after: u64) {
        let method = resp.cseq().method().as_str();
        if resp.status() < 200 || method.eq_ignore_ascii_case("INVITE") {
            return;
        }
        let Some(key) = Key::of_response(resp) else { return };
        let mut txns = self.txns.lock().unwrap();
        if matches!(txns.get(&key), Some(State::Completed { .. })) {
            return;
        }
        let until = Instant::now() + TIMER_J;
        txns.insert(key, State::Completed { wire: wire.to_vec(), dst, until, after });
    }
}
