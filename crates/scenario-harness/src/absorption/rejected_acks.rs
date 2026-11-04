//! RFC 3261 §17.1.1.2 for this endpoint's INVITE client transactions in
//! Completed: the hop ACK each sent for a non-2xx final (§17.1.1.3), which a
//! repeat of that final draws again.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;

use sip_message::{Method, SipRequest, SipResponse};

/// An INVITE client transaction's identity as its ACK and final carry it:
/// Call-ID, CSeq number and top-Via branch.
type Key = (String, u32, String);

/// The hop ACKs this endpoint sent, each with its wire image and destination.
#[derive(Default)]
pub(super) struct RejectedAcks {
    acks: Mutex<HashMap<Key, (Vec<u8>, SocketAddr)>>,
}

impl RejectedAcks {
    /// Note `ack`, sent as `wire` to `dst`. A 2xx's ACK takes a branch of its
    /// own, so no final ever matches it.
    pub(super) fn note_sent(&self, ack: &SipRequest, wire: &[u8], dst: SocketAddr) {
        if ack.method() != Method::Ack {
            return;
        }
        let Some(branch) = ack.top_via().branch().filter(|b| !b.is_empty()) else { return };
        let key = (ack.call_id().as_str().to_string(), ack.cseq().seq(), branch.to_string());
        self.acks.lock().unwrap().insert(key, (wire.to_vec(), dst));
    }

    /// The ACK, and where it went, that `resp` drew when it is a non-2xx
    /// final to an INVITE this endpoint already ACKed.
    pub(super) fn ack_for(&self, resp: &SipResponse) -> Option<(Vec<u8>, SocketAddr)> {
        if resp.status() < 300 || resp.cseq().method() != Method::Invite {
            return None;
        }
        let branch = resp.top_via().branch()?;
        let key = (resp.call_id().as_str().to_string(), resp.cseq().seq(), branch.to_string());
        self.acks.lock().unwrap().get(&key).cloned()
    }
}
