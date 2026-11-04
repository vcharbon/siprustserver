//! The datagrams the loss model dropped before their taker: the evidence that
//! a sender sent what its taker's lane never shows.

use std::collections::HashSet;

use layer_harness::{LaneKey, Stamped};

use crate::contracts::SignalingNetworkEvent;
use crate::rfc_audit::msg_reads::call_id;
use crate::types::RecvDisposition;

/// Per taker lane, sender address and Call-ID, that the loss model dropped a
/// datagram the sender sent that lane on that call.
pub(crate) struct Dropped(HashSet<(LaneKey, String, String)>);

impl Dropped {
    pub(crate) fn of(events: &[Stamped<SignalingNetworkEvent>]) -> Dropped {
        let mut dropped = HashSet::new();
        for s in events {
            let SignalingNetworkEvent::RecvItem { bind_key, packet, wire, disposition } = &s.event
            else {
                continue;
            };
            if *disposition != RecvDisposition::LossModel {
                continue;
            }
            let Some(parsed) = wire.parsed.as_deref() else { continue };
            dropped.insert((bind_key.clone(), packet.src.to_string(), call_id(parsed).to_string()));
        }
        Dropped(dropped)
    }

    /// Whether the loss model dropped a datagram `sender` sent `taker` on the
    /// call `call_id`.
    pub(crate) fn holds(&self, taker: &str, sender: &str, call_id: &str) -> bool {
        self.0.contains(&(taker.to_string(), sender.to_string(), call_id.to_string()))
    }
}
