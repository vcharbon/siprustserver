//! One datagram out of a UA, and what the transaction layer
//! ([`Absorption`](crate::absorption::Absorption)) sends again: the finals a
//! copy of an answered request draws (RFC 3261 §17.2.1, §17.2.2), the finals
//! to an INVITE until their ACK (§13.3.1.4, §17.2.1), the requests until
//! their answer (§17.1.1.2, §17.1.2.2), and the ACK a final draws again.

use std::net::SocketAddr;

use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipResponse};

use super::step::StepError;
use super::Agent;

impl Agent {
    /// One datagram out, nothing fed but its record in the transaction layer
    /// ([`Absorption::note_sent`](crate::absorption::Absorption::note_sent)).
    pub(super) async fn send_bytes(&self, wire: &[u8], dst: SocketAddr) -> Result<(), StepError> {
        if let Ok(msg) = CustomParser::new().parse(wire) {
            self.txn.note_sent(&msg, wire, dst, self.ep.queue_depth());
        }
        self.ep
            .send_to(wire, dst)
            .await
            .map_err(|e| StepError::Transport { who: self.name.clone(), detail: e.to_string() })
    }

    /// Re-send the ACK the final `resp` draws, and say whether one left: a
    /// final to an INVITE given up on (§17.1.1.3), and, when `repeat`, a
    /// non-2xx final to this UA's INVITE its client transaction already ACKed
    /// (§17.1.1.2) or a 2xx this UA's core already ACKed (§13.2.2.4) — the TU
    /// acting on a datagram the TU view keeps.
    pub(super) async fn re_ack(&self, resp: &SipResponse, repeat: bool) -> bool {
        if let Some((ack, dst)) = self.txn.abandoned_ack_for(resp) {
            let _ = self.try_send(&SipMessage::Request(ack), dst).await;
            return true;
        }
        if !repeat {
            return false;
        }
        if let Some((ack, dst)) = self.txn.rejected_ack_for(resp) {
            let _ = self.send_bytes(&ack, dst).await;
            return true;
        }
        if let Some((ack, dst)) = self.two_xx_acks.owed_for(resp) {
            let _ = self.try_send(&SipMessage::Request(ack), dst).await;
            return true;
        }
        false
    }

    /// Send the finals the copies of completed transactions just sighted
    /// draw again (RFC 3261 §17.2.2).
    pub(super) async fn send_repeats(&self) {
        for (wire, dst) in self.txn.take_repeats() {
            let _ = self.send_bytes(&wire, dst).await;
        }
    }

    /// When a datagram this UA sent is due again for want of its answer or
    /// its ACK. Only the actor layer drives it: a hand-scripted body sends
    /// each retransmission itself.
    pub(crate) fn next_due(&self) -> Option<tokio::time::Instant> {
        self.txn.next_due()
    }

    /// Send again every datagram due for want of its answer or its ACK.
    pub(crate) async fn send_due(&self) {
        for (wire, dst) in self.txn.take_due(tokio::time::Instant::now()) {
            let _ = self.send_bytes(&wire, dst).await;
        }
    }

    /// This UA plays a peer that never retransmits: an actor driving it
    /// sends no request again for want of its answer and no final for want
    /// of its ACK.
    pub fn silence_retransmissions(&self) {
        self.txn.silence_retransmissions();
    }
}
