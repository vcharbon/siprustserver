//! This endpoint's finals to an INVITE, passed to the transport again after
//! T1 and doubling up to T2 until their ACK arrives or 64·T1 passed: a 2xx by
//! the UAS core (RFC 3261 §13.3.1.4), a non-2xx by the INVITE server
//! transaction on Timer G until Timer H (§17.2.1).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;

use sip_message::{SipRequest, SipResponse};
use tokio::time::Instant;

/// RFC 3261 T1.
const T1: Duration = Duration::from_millis(500);
/// RFC 3261 T2: the cap of the interval.
const T2: Duration = Duration::from_secs(4);

/// The final's dialog and number: Call-ID, the UAS's To-tag, the CSeq.
type Key = (String, String, u32);

/// One final awaiting its ACK.
struct Pending {
    wire: Vec<u8>,
    dst: SocketAddr,
    next: Instant,
    interval: Duration,
    until: Instant,
}

/// The per-endpoint table of INVITE finals awaiting their ACK.
#[derive(Default)]
pub(super) struct UnackedFinals {
    pending: Mutex<HashMap<Key, Pending>>,
}

impl UnackedFinals {
    /// Note `r`, rendered as `wire` toward `dst`, when it is a final to an
    /// INVITE; a repeat of one already noted keeps its ladder.
    pub(super) fn note_sent(&self, r: &SipResponse, wire: &[u8], dst: SocketAddr) {
        let invite_final = r.status() >= 200 && r.cseq().method().as_str() == "INVITE";
        let Some(tag) = r.to().tag().filter(|_| invite_final) else { return };
        let key = (r.call_id().to_string(), tag.to_string(), r.cseq().seq());
        let now = Instant::now();
        self.pending.lock().unwrap().entry(key).or_insert_with(|| Pending {
            wire: wire.to_vec(),
            dst,
            next: now + T1,
            interval: T1,
            until: now + 64 * T1,
        });
    }

    /// The ACK `r` ends the ladder of the final it acknowledges; a BYE ends
    /// every ladder of the dialog it ends, whose session no ACK can still
    /// confirm (RFC 3261 §15).
    pub(super) fn sight(&self, r: &SipRequest) {
        let Some(tag) = r.to().tag() else { return };
        let call_id = r.call_id().to_string();
        match r.method().as_str() {
            "ACK" => {
                let key = (call_id, tag.to_string(), r.cseq().seq());
                self.pending.lock().unwrap().remove(&key);
            }
            "BYE" => self.end_dialog(&call_id, tag),
            _ => {}
        }
    }

    /// This endpoint sent `r`: a BYE ends every ladder of its dialog, this
    /// endpoint's tag being its From-tag.
    pub(super) fn note_request_sent(&self, r: &SipRequest) {
        if r.method().as_str() == "BYE" {
            if let Some(tag) = r.from().tag() {
                self.end_dialog(r.call_id().as_str(), tag);
            }
        }
    }

    /// End every ladder of the dialog `call_id` and this endpoint's `tag`
    /// name.
    fn end_dialog(&self, call_id: &str, tag: &str) {
        self.pending.lock().unwrap().retain(|(c, t, _), _| !(c == call_id && t == tag));
    }

    /// When the next final is due again, if one awaits its ACK.
    pub(super) fn next_due(&self) -> Option<Instant> {
        self.pending.lock().unwrap().values().map(|p| p.next).min()
    }

    /// The finals due again at `now`, each with its destination; a final
    /// past 64·T1 is given up.
    pub(super) fn take_due(&self, now: Instant) -> Vec<(Vec<u8>, SocketAddr)> {
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|_, p| p.until > now);
        let mut due = Vec::new();
        for p in pending.values_mut().filter(|p| p.next <= now) {
            due.push((p.wire.clone(), p.dst));
            p.interval = (p.interval * 2).min(T2);
            p.next = now + p.interval;
        }
        due
    }

    /// Whether a final still awaits its ACK at `now`.
    pub(super) fn awaits_an_ack(&self, now: Instant) -> bool {
        self.pending.lock().unwrap().values().any(|p| p.until > now)
    }
}
