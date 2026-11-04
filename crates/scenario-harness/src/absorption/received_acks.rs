//! RFC 3261 §13.3.1.4 for the ACKs this endpoint receives: one ACK of a 2xx
//! confirms the dialog, and a repeat of it — the peer re-ACKing, or a node
//! re-sending the ACK its record holds — is the UAS core's to discard.

use std::collections::HashMap;
use std::sync::Mutex;

use sip_message::{Method, SipRequest};

/// The ACKs received, by Call-ID and CSeq number, with how many arrived.
#[derive(Default)]
pub(super) struct ReceivedAcks {
    seen: Mutex<HashMap<(String, u32), u32>>,
}

impl ReceivedAcks {
    /// Count `r` when it is an ACK.
    pub(super) fn sight(&self, r: &SipRequest) {
        if r.method() == Method::Ack {
            let key = (r.call_id().as_str().to_string(), r.cseq().seq());
            *self.seen.lock().unwrap().entry(key).or_default() += 1;
        }
    }

    /// Whether `r` is an ACK that arrived before it.
    pub(super) fn repeats(&self, r: &SipRequest) -> bool {
        let key = (r.call_id().as_str().to_string(), r.cseq().seq());
        r.method() == Method::Ack && self.seen.lock().unwrap().get(&key).is_some_and(|n| *n > 1)
    }
}
