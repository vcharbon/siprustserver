//! The datagrams the loss model dropped between a lane and a party: the
//! evidence that the lane's view of its traffic with that party is incomplete.

use std::collections::HashSet;

use layer_harness::{LaneKey, Stamped};
use sip_message::SipParser;

use crate::contracts::SignalingNetworkEvent;
use crate::rfc_audit::msg_reads::call_id;
use crate::types::RecvDisposition;

/// Per lane, party address and Call-ID, that the loss model dropped a
/// datagram between them on that call: one the party sent the lane, dropped
/// on arrival, or one the lane sent the party, dropped on send.
pub(crate) struct Dropped(HashSet<(LaneKey, String, String)>);

impl Dropped {
    pub(crate) fn of(events: &[Stamped<SignalingNetworkEvent>]) -> Dropped {
        let mut dropped = HashSet::new();
        for s in events {
            match &s.event {
                SignalingNetworkEvent::RecvItem {
                    bind_key,
                    packet,
                    wire,
                    disposition: RecvDisposition::LossModel,
                } => {
                    let Some(parsed) = wire.parsed.as_deref() else { continue };
                    let cid = call_id(parsed).to_string();
                    dropped.insert((bind_key.clone(), packet.src.to_string(), cid));
                }
                SignalingNetworkEvent::SendLost { bind_key, to, msg } => {
                    let Ok(parsed) = crate::rfc_audit::lenient_parser().parse(msg) else {
                        continue;
                    };
                    dropped.insert((
                        bind_key.clone(),
                        to.to_string(),
                        call_id(&parsed).to_string(),
                    ));
                }
                _ => {}
            }
        }
        Dropped(dropped)
    }

    /// Whether the loss model dropped a datagram between `lane` and `party`
    /// on the call `call_id`, in either direction.
    pub(crate) fn holds(&self, lane: &str, party: &str, call_id: &str) -> bool {
        self.0.contains(&(lane.to_string(), party.to_string(), call_id.to_string()))
    }
}
