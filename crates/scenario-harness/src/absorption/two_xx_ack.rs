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

    /// The ACK owed to `resp`, a repeat of a 2xx to an INVITE whose dialog
    /// this core already confirmed. A non-2xx after the 2xx owes none: the
    /// UAC discards it (RFC 6026 §8.4).
    pub(crate) fn owed_for(&self, resp: &SipResponse) -> Option<(SipRequest, SocketAddr)> {
        let key = ack_key_of_final(resp)?;
        self.sent.lock().unwrap().get(&key).cloned()
    }
}

#[cfg(test)]
mod tests {
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};

    use super::*;

    fn message(text: &str) -> SipMessage {
        CustomParser::new().parse(text.as_bytes()).expect("a test message")
    }

    fn final_(status: u16) -> SipResponse {
        let text = format!(
            "SIP/2.0 {status} Whatever\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-i\r\n\
             From: <sip:a@x>;tag=a\r\nTo: <sip:b@x>;tag=b\r\nCall-ID: c1\r\n\
             CSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
        );
        match message(&text) {
            SipMessage::Response(r) => r,
            SipMessage::Request(_) => unreachable!(),
        }
    }

    /// A non-2xx after the 2xx this core ACKed draws no ACK: the cached one
    /// answers a repeat of the 2xx alone.
    #[test]
    fn a_non_2xx_after_the_2xx_owes_no_ack() {
        let ack = "ACK sip:b@x SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-k\r\n\
                   From: <sip:a@x>;tag=a\r\nTo: <sip:b@x>;tag=b\r\nCall-ID: c1\r\n\
                   CSeq: 1 ACK\r\nContent-Length: 0\r\n\r\n";
        let SipMessage::Request(ack) = message(ack) else { unreachable!() };
        let acks = TwoXxAcks::default();
        let dst = "127.0.0.1:5080".parse().unwrap();
        acks.remember("c1".into(), 1, "b".into(), ack, dst);
        assert!(acks.owed_for(&final_(200)).is_some(), "a repeat of the 2xx is re-ACKed");
        assert!(acks.owed_for(&final_(487)).is_none(), "a 487 after it is discarded");
    }
}
