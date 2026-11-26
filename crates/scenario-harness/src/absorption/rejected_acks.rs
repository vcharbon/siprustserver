//! RFC 3261 §17.1.1.2 for this endpoint's INVITE client transactions in
//! Completed: the hop ACK each sent for a non-2xx final (§17.1.1.3), which a
//! repeat of that final draws again until Timer D (32 s over UDP) ends the
//! transaction.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;

use sip_message::{Method, SipRequest, SipResponse};
use tokio::time::Instant;

/// Timer D over UDP: 32 s (RFC 3261 §17.1.1.2).
const TIMER_D: Duration = Duration::from_secs(32);

/// An INVITE client transaction's identity as its ACK and final carry it:
/// Call-ID, CSeq number and top-Via branch.
type Key = (String, u32, String);

/// One hop ACK sent: its wire image, its destination, and when Timer D ends
/// its transaction.
struct Sent {
    wire: Vec<u8>,
    dst: SocketAddr,
    until: Instant,
}

/// The hop ACKs this endpoint sent.
#[derive(Default)]
pub(super) struct RejectedAcks {
    acks: Mutex<HashMap<Key, Sent>>,
}

impl RejectedAcks {
    /// Note `ack`, sent as `wire` to `dst`: the first ACK of a transaction
    /// starts its Timer D, a repeat keeps it. A 2xx's ACK takes a branch of
    /// its own, so no final ever matches it.
    pub(super) fn note_sent(&self, ack: &SipRequest, wire: &[u8], dst: SocketAddr) {
        if ack.method() != Method::Ack {
            return;
        }
        let Some(branch) = ack.top_via().branch().filter(|b| !b.is_empty()) else { return };
        let key = (ack.call_id().as_str().to_string(), ack.cseq().seq(), branch.to_string());
        let until = Instant::now() + TIMER_D;
        self.acks.lock().unwrap().entry(key).or_insert_with(|| Sent {
            wire: wire.to_vec(),
            dst,
            until,
        });
    }

    /// The ACK, and where it went, that `resp` drew when it is a non-2xx
    /// final to an INVITE this endpoint already ACKed, within that
    /// transaction's Timer D.
    pub(super) fn ack_for(&self, resp: &SipResponse) -> Option<(Vec<u8>, SocketAddr)> {
        if resp.status() < 300 || resp.cseq().method() != Method::Invite {
            return None;
        }
        let branch = resp.top_via().branch()?;
        let key = (resp.call_id().as_str().to_string(), resp.cseq().seq(), branch.to_string());
        let acks = self.acks.lock().unwrap();
        let sent = acks.get(&key).filter(|s| Instant::now() < s.until)?;
        Some((sent.wire.clone(), sent.dst))
    }
}
