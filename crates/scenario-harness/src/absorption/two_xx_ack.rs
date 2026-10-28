//! The UAC core's ACK cache (RFC 3261 §13.2.2.4 / §17.1.1.3): the ACK to a 2xx
//! is a dialog-level message the **core** sends, once per 2xx it receives —
//! the INVITE client transaction terminated on the first one and absorbs
//! nothing thereafter.
//!
//! So a retransmitted 2xx stays in the TU view (see the table in
//! [`super`]) and the core answers it with the same ACK again, byte for byte,
//! for 64·T1 after it sent the ACK: the UAS gives up retransmitting its 2xx
//! 64·T1 after the first one (§13.3.1.4), which the ACK follows. Past that the
//! ACK's bytes go and its key stays: a later repeat of that 2xx matches nothing
//! owed and is discarded (RFC 6026 §8.4, §18.1.2), never surfaced. Keyed
//! `(Call-ID, INVITE CSeq number, To-tag)`: the To-tag is in the key because a
//! forked 2xx confirms its own dialog and carries its own ACK.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;

use sip_message::{SipRequest, SipResponse};
use tokio::time::Instant;

/// How long a sent ACK stays owed to a repeat of its 2xx: 64·T1.
const OWED_FOR: Duration = Duration::from_millis(64 * 500);

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

/// One ACK sent to a 2xx: its wire image and where it went while a repeat of
/// the 2xx draws it again (`None` past that), and until when.
struct SentAck {
    wire: Option<(Box<[u8]>, SocketAddr)>,
    until: Instant,
}

/// What a repeat of a 2xx this core ACKed draws.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Owed {
    /// The ACK again: its wire image and where it went.
    Resend(Vec<u8>, SocketAddr),
    /// Nothing: the repeat came past 64·T1 and is discarded.
    Discard,
}

/// The per-endpoint cache of ACKs sent to 2xx finals. An entry's bytes past
/// its 64·T1 are let go at the next use of the cache; its key stays.
#[derive(Default)]
pub(crate) struct TwoXxAcks {
    sent: Mutex<HashMap<AckKey, SentAck>>,
}

impl TwoXxAcks {
    /// Remember `ack`, sent as `wire` to `dst`. An ACK without a To-tag
    /// confirms no dialog and is not kept.
    pub(crate) fn remember(&self, ack: &SipRequest, wire: &[u8], dst: SocketAddr) {
        let Some(key) = ack_key_of_ack(ack) else { return };
        let now = Instant::now();
        let mut sent = self.sent.lock().unwrap();
        release_expired(&mut sent, now);
        sent.insert(key, SentAck { wire: Some((wire.into(), dst)), until: now + OWED_FOR });
    }

    /// What `resp` draws when it is a 2xx to an INVITE whose dialog this core
    /// confirmed: the ACK again within 64·T1 of it, nothing past that. `None`
    /// for any other response: a non-2xx after the 2xx owes no ACK, the UAC
    /// discards it (RFC 6026 §8.4).
    pub(crate) fn owed_for(&self, resp: &SipResponse) -> Option<Owed> {
        let key = ack_key_of_final(resp)?;
        let mut sent = self.sent.lock().unwrap();
        release_expired(&mut sent, Instant::now());
        sent.get(&key).map(|a| match &a.wire {
            Some((wire, dst)) => Owed::Resend(wire.to_vec(), *dst),
            None => Owed::Discard,
        })
    }

    /// How many ACKs the cache holds the bytes of.
    #[cfg(test)]
    fn held(&self) -> usize {
        self.sent.lock().unwrap().values().filter(|a| a.wire.is_some()).count()
    }
}

/// Let go of the bytes of every ACK past its 64·T1.
fn release_expired(sent: &mut HashMap<AckKey, SentAck>, now: Instant) {
    for a in sent.values_mut().filter(|a| a.until <= now) {
        a.wire = None;
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
        acks.remember(&ack, b"the ACK's bytes", dst);
        assert!(acks.owed_for(&final_(200)).is_some(), "a repeat of the 2xx is re-ACKed");
        assert!(acks.owed_for(&final_(487)).is_none(), "a 487 after it is discarded");
    }

    /// A repeat of the 2xx draws the ACK's own bytes, to where it went, for
    /// 64·T1 after it left; past that the bytes go and the repeat is discarded.
    #[tokio::test(start_paused = true)]
    async fn the_cached_ack_is_its_sent_bytes_until_64_t1() {
        let wire = "ACK sip:b@x SIP/2.0\r\nv: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-k\r\n\
                    f: <sip:a@x>;tag=a\r\nt: <sip:b@x>;tag=b\r\ni: c1\r\n\
                    CSeq:  1 ACK\r\nl: 0\r\n\r\n";
        let SipMessage::Request(ack) = message(wire) else { unreachable!() };
        let acks = TwoXxAcks::default();
        let dst: SocketAddr = "127.0.0.1:5080".parse().unwrap();
        acks.remember(&ack, wire.as_bytes(), dst);

        tokio::time::advance(std::time::Duration::from_millis(64 * 500 - 1)).await;
        let owed = acks.owed_for(&final_(200));
        let sent = Owed::Resend(wire.as_bytes().to_vec(), dst);
        assert_eq!(owed, Some(sent), "the sent bytes, byte for byte");

        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        assert_eq!(acks.owed_for(&final_(200)), Some(Owed::Discard), "past 64·T1 it is discarded");
        assert_eq!(acks.held(), 0, "the bytes are let go");
    }
}
