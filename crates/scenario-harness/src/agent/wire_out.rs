//! One datagram out of a UA, and the finals of its non-INVITE server
//! transactions the §17.2 receive view ([`Absorption`](crate::absorption::Absorption))
//! records and repeats (RFC 3261 §17.2.2).

use std::net::SocketAddr;

use sip_message::parser::custom::CustomParser;
use sip_message::SipParser;

use super::step::StepError;
use super::Agent;

impl Agent {
    /// One datagram out, nothing fed but the record of a final it carries
    /// to a non-INVITE request ([`Absorption::note_sent`](crate::absorption::Absorption::note_sent)).
    pub(super) async fn send_bytes(&self, wire: &[u8], dst: SocketAddr) -> Result<(), StepError> {
        if wire.starts_with(b"SIP/2.0 ") {
            if let Ok(msg) = CustomParser::new().parse(wire) {
                self.txn.note_sent(&msg, wire, dst, self.ep.queue_depth());
            }
        }
        self.ep
            .send_to(wire, dst)
            .await
            .map_err(|e| StepError::Transport { who: self.name.clone(), detail: e.to_string() })
    }

    /// Send the finals the copies of completed transactions just sighted
    /// draw again (RFC 3261 §17.2.2).
    pub(super) async fn send_repeats(&self) {
        for (wire, dst) in self.txn.take_repeats() {
            let _ = self.send_bytes(&wire, dst).await;
        }
    }

    /// When a 2xx this UA sent is due again for want of its ACK (RFC 3261
    /// §13.3.1.4).
    pub(crate) fn next_2xx_due(&self) -> Option<tokio::time::Instant> {
        self.txn.next_2xx_due()
    }

    /// Send again every 2xx due for want of its ACK (RFC 3261 §13.3.1.4).
    pub(crate) async fn send_due_2xx(&self) {
        for (wire, dst) in self.txn.take_due_2xx(tokio::time::Instant::now()) {
            let _ = self.send_bytes(&wire, dst).await;
        }
    }
}
