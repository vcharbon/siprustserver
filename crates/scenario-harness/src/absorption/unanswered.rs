//! RFC 3261 §17.1.1.2 and §17.1.2.2 for this endpoint's client transactions
//! over an unreliable transport: a request other than ACK goes again on its
//! timer until it is answered — an INVITE on Timer A (after T1, doubling)
//! until any response, any other request on Timer E (after T1, doubling up
//! to T2; T2 once a provisional came) until its final — and is given up at
//! 64·T1 (Timer B, Timer F).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;

use sip_message::{SipRequest, SipResponse};
use tokio::time::Instant;

/// RFC 3261 T1.
const T1: Duration = Duration::from_millis(500);
/// RFC 3261 T2: the cap of a non-INVITE request's interval.
const T2: Duration = Duration::from_secs(4);

/// A client transaction: its top-Via branch and its method (§17.1.3).
type Key = (String, String);

/// One request awaiting its answer.
struct Pending {
    wire: Vec<u8>,
    dst: SocketAddr,
    invite: bool,
    next: Instant,
    interval: Duration,
    until: Instant,
}

/// The per-endpoint table of requests awaiting their answer.
#[derive(Default)]
pub(super) struct Unanswered {
    pending: Mutex<HashMap<Key, Pending>>,
}

impl Unanswered {
    /// Note `r`, rendered as `wire` toward `dst`, unless it is an ACK; a
    /// request of a transaction already noted keeps its ladder.
    pub(super) fn note_sent(&self, r: &SipRequest, wire: &[u8], dst: SocketAddr) {
        let method = r.method().as_str();
        if method == "ACK" {
            return;
        }
        let Some(branch) = r.top_via().branch().filter(|b| !b.is_empty()) else { return };
        let key = (branch.to_string(), method.to_string());
        let now = Instant::now();
        self.pending.lock().unwrap().entry(key).or_insert_with(|| Pending {
            wire: wire.to_vec(),
            dst,
            invite: method == "INVITE",
            next: now + T1,
            interval: T1,
            until: now + 64 * T1,
        });
    }

    /// The response `r` ends the ladder of the request it answers: an
    /// INVITE's on any response, any other request's on its final; a
    /// provisional to a non-INVITE request sets its interval to T2.
    pub(super) fn sight(&self, r: &SipResponse) {
        let Some(branch) = r.top_via().branch() else { return };
        let key = (branch.to_string(), r.cseq().method().as_str().to_string());
        let mut pending = self.pending.lock().unwrap();
        let Some(p) = pending.get_mut(&key) else { return };
        if p.invite || r.status() >= 200 {
            pending.remove(&key);
        } else if p.interval < T2 {
            p.interval = T2;
            p.next = Instant::now() + T2;
        }
    }

    /// When the next request is due again, if one awaits its answer.
    pub(super) fn next_due(&self) -> Option<Instant> {
        self.pending.lock().unwrap().values().map(|p| p.next).min()
    }

    /// The requests due again at `now`, each with its destination; a request
    /// past 64·T1 is given up.
    pub(super) fn take_due(&self, now: Instant) -> Vec<(Vec<u8>, SocketAddr)> {
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|_, p| p.until > now);
        let mut due = Vec::new();
        for p in pending.values_mut().filter(|p| p.next <= now) {
            due.push((p.wire.clone(), p.dst));
            p.interval = if p.invite { p.interval * 2 } else { (p.interval * 2).min(T2) };
            p.next = now + p.interval;
        }
        due
    }
}
