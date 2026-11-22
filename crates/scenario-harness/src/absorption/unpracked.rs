//! RFC 3262 §3 for this endpoint's reliable provisionals: each is passed to
//! the transport again after T1, the interval doubling with no cap, until the
//! PRACK its `RAck` names arrives, and given up at 64·T1 (the UAS core then
//! rejects the INVITE 5xx). A final to the INVITE ends every ladder of its
//! transaction, which passes no provisional on once it answered (RFC 3261
//! §17.2.1).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;

use sip_message::header::{RAck, RSeq};
use sip_message::{SipRequest, SipResponse};
use tokio::time::Instant;

/// RFC 3261 T1.
const T1: Duration = Duration::from_millis(500);

/// A reliable provisional's identity as its PRACK names it: Call-ID, the
/// UAS's To-tag, the INVITE's CSeq number and the provisional's RSeq.
type Key = (String, String, u32, u32);

/// One reliable provisional awaiting its PRACK.
struct Pending {
    wire: Vec<u8>,
    dst: SocketAddr,
    next: Instant,
    interval: Duration,
    until: Instant,
}

/// The per-endpoint table of reliable provisionals awaiting their PRACK.
#[derive(Default)]
pub(super) struct Unpracked {
    pending: Mutex<HashMap<Key, Pending>>,
}

impl Unpracked {
    /// Note `r`, rendered as `wire` toward `dst`: a reliable provisional to an
    /// INVITE starts its ladder, a repeat of one already noted keeps it; a
    /// final to an INVITE ends every ladder of its transaction.
    pub(super) fn note_sent(&self, r: &SipResponse, wire: &[u8], dst: SocketAddr) {
        if r.cseq().method().as_str() != "INVITE" {
            return;
        }
        let (call_id, seq) = (r.call_id().to_string(), r.cseq().seq());
        if r.status() >= 200 {
            let mut pending = self.pending.lock().unwrap();
            pending.retain(|(c, _, s, _), _| !(*c == call_id && *s == seq));
            return;
        }
        let rseq = r.header::<RSeq>().and_then(Result::ok).map(|v| v.value());
        let (Some(rseq), Some(tag)) = (rseq, r.to().tag()) else { return };
        let now = Instant::now();
        let key = (call_id, tag.to_string(), seq, rseq);
        self.pending.lock().unwrap().entry(key).or_insert_with(|| Pending {
            wire: wire.to_vec(),
            dst,
            next: now + T1,
            interval: T1,
            until: now + 64 * T1,
        });
    }

    /// The PRACK `r` ends the ladder of the provisional its `RAck` names on
    /// its dialog.
    pub(super) fn sight(&self, r: &SipRequest) {
        if r.method().as_str() != "PRACK" {
            return;
        }
        let Some(rack) = r.header::<RAck>().and_then(Result::ok) else { return };
        let Some(tag) = r.to().tag() else { return };
        if rack.method().as_str() != "INVITE" {
            return;
        }
        let key = (r.call_id().to_string(), tag.to_string(), rack.seq(), rack.rseq());
        self.pending.lock().unwrap().remove(&key);
    }

    /// When the next provisional is due again, if one awaits its PRACK.
    pub(super) fn next_due(&self) -> Option<Instant> {
        self.pending.lock().unwrap().values().map(|p| p.next).min()
    }

    /// The provisionals due again at `now`, each with its destination; one
    /// past 64·T1 is given up.
    pub(super) fn take_due(&self, now: Instant) -> Vec<(Vec<u8>, SocketAddr)> {
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|_, p| p.until > now);
        let mut due = Vec::new();
        for p in pending.values_mut().filter(|p| p.next <= now) {
            due.push((p.wire.clone(), p.dst));
            p.interval *= 2;
            p.next = now + p.interval;
        }
        due
    }
}
