//! The UAC core's ACK cache (RFC 3261 §13.2.2.4 / §17.1.1.3): the ACK to a 2xx
//! is a dialog-level message the **core** sends, once per 2xx it receives —
//! the INVITE client transaction terminated on the first one and absorbs
//! nothing thereafter.
//!
//! So a retransmitted 2xx stays in the TU view (see the table in
//! [`super`]) and the core answers it with the same ACK again. Keyed
//! `(Call-ID, INVITE CSeq number, To-tag)`: the To-tag is in the key because a
//! forked 2xx confirms its own dialog and carries its own ACK.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;

use sip_message::{SipRequest, SipResponse};

/// The dialog one ACK-to-2xx belongs to: Call-ID, the INVITE CSeq NUMBER and
/// the To-tag. One key, two readings — the 2xx that draws the ACK and the ACK
/// that answers it — so a driver pairing them can never key on a tuple of its
/// own.
pub type AckKey = (String, u32, String);

/// The key `resp` draws its ACK under. `None` unless `resp` is a 2xx to an
/// INVITE carrying a To-tag: no other response draws a core-sent ACK.
pub fn ack_key_of_final(resp: &SipResponse) -> Option<AckKey> {
    if !(200..300).contains(&resp.status()) || resp.cseq().method().as_str() != "INVITE" {
        return None;
    }
    Some((resp.call_id().to_string(), resp.cseq().seq(), resp.to().tag()?.to_string()))
}

/// The key `req` — an ACK — answers. `None` unless it is an ACK with a To-tag,
/// which is every ACK to a 2xx (RFC 3261 §13.2.2.4) and no ACK to a non-2xx of
/// a dialog this core never confirmed.
pub fn ack_key_of_ack(req: &SipRequest) -> Option<AckKey> {
    if req.method().as_str() != "ACK" {
        return None;
    }
    Some((req.call_id().to_string(), req.cseq().seq(), req.to().tag()?.to_string()))
}

/// The per-endpoint cache of ACKs sent to 2xx finals.
#[derive(Default)]
pub(crate) struct TwoXxAcks {
    sent: Mutex<HashMap<AckKey, (SipRequest, SocketAddr)>>,
}

impl TwoXxAcks {
    /// Remember the ACK this core just sent for a confirmed dialog.
    pub(crate) fn remember(
        &self,
        call_id: String,
        cseq: u32,
        to_tag: String,
        ack: SipRequest,
        dst: SocketAddr,
    ) {
        self.sent.lock().unwrap().insert((call_id, cseq, to_tag), (ack, dst));
    }

    /// The ACK owed to `resp`, if this core already confirmed that dialog.
    pub(crate) fn owed_for(&self, resp: &SipResponse) -> Option<(SipRequest, SocketAddr)> {
        let to_tag = resp.to().tag()?.to_string();
        let key = (resp.call_id().to_string(), resp.cseq().seq(), to_tag);
        self.sent.lock().unwrap().get(&key).cloned()
    }
}
