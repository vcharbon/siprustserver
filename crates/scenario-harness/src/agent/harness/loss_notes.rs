//! The datagrams a run's fabric loses, noted for the call its parties play:
//! on a run that declares a loss model, each `SendLost` to or from a party
//! becomes the [`LostMessage`] the call's goals and endings read
//! ([`LossNotes`]), as the load lane's loss model notes it.

use std::collections::HashMap;
use std::net::SocketAddr;

use layer_harness::Stamped;
use sip_message::sniff;
use sip_net::RecvDisposition;
use sip_net_testkit::report::lane_addr;
use sip_net_testkit::SignalingNetworkEvent;

use super::Harness;
use crate::dialogs::send_lost;
use crate::realcall::{LossNotes, LostMessage};
use crate::Agent;

impl Harness {
    /// On a fabric that runs a loss model, note in `notes` each datagram the
    /// fabric loses on its way from or to a party of the call, as the
    /// recording records its loss: `caller` is the call's caller, each of
    /// `callees` the callee leg its agent's name names. Each party has a
    /// socket of its own; a loss between two parties is noted for both. On
    /// any other run nothing is noted. Replaces an earlier noting.
    pub fn note_losses(&self, notes: LossNotes, caller: &Agent, callees: &[&Agent]) {
        if !self.lossy {
            return;
        }
        let parties: HashMap<SocketAddr, Option<String>> = std::iter::once((caller.addr(), None))
            .chain(callees.iter().map(|a| (a.addr(), Some(a.name().to_string()))))
            .collect();
        let channel = self.recording.channel();
        self.recording.watch_losses(move |event| {
            let SignalingNetworkEvent::SendLost { bind_key, to, msg } = event else { return };
            let events = channel.snapshot();
            let ends = [(lane_addr(bind_key), false), (Some(*to), true)];
            for (party, inbound) in ends {
                let Some((addr, leg)) = party.and_then(|a| parties.get_key_value(&a)) else {
                    continue;
                };
                let arrived = arrived_before(&events, event, *addr, inbound);
                notes.note(lost_message(msg, leg.clone(), inbound, arrived));
            }
        });
    }
}

/// The note of the lost datagram `msg`, to (`inbound`) or from the party of
/// `leg`.
fn lost_message(
    msg: &[u8],
    leg: Option<String>,
    inbound: bool,
    arrived_before: Vec<(u16, String)>,
) -> LostMessage {
    let id = sniff::identity(msg);
    LostMessage {
        leg,
        inbound,
        method: id.method.map(str::to_string),
        status: id.status,
        cseq_method: id.cseq_method.unwrap_or_default().to_string(),
        sdp: sniff::session_description(msg).is_some(),
        reliable: id.reliable,
        to_tag: id.to_tag.unwrap_or_default().to_string(),
        call_id: id.call_id.unwrap_or_default().to_string(),
        from_tag: sniff::from_tag(msg),
        cseq: id.cseq.unwrap_or_default(),
        arrived_before,
    }
}

/// The responses that reach `party` ahead of the datagram the `SendLost`
/// `lost` names, each distinct `(status, CSeq method)` once, in record
/// order: those delivered to it before the loss, and, for a datagram lost on
/// its way to it (`inbound`), those sent to it before and not lost, which a
/// network that loses at arrival delivers first unless it held them.
fn arrived_before(
    events: &[Stamped<SignalingNetworkEvent>],
    lost: &SignalingNetworkEvent,
    party: SocketAddr,
    inbound: bool,
) -> Vec<(u16, String)> {
    let lost_at = events.iter().rposition(|s| s.event == *lost).unwrap_or(events.len());
    let mut arrived: Vec<(u16, String)> = Vec::new();
    for (i, s) in events[..lost_at].iter().enumerate() {
        let response = match &s.event {
            SignalingNetworkEvent::RecvItem { bind_key, packet, disposition, .. }
                if *disposition == RecvDisposition::Delivered
                    && lane_addr(bind_key) == Some(party) =>
            {
                &packet.raw[..]
            }
            SignalingNetworkEvent::SendCalled { to, msg, .. }
                if inbound && *to == party && !send_lost(events, i) =>
            {
                &msg[..]
            }
            _ => continue,
        };
        let id = sniff::identity(response);
        let (Some(status), Some(method)) = (id.status, id.cseq_method) else { continue };
        if !arrived.iter().any(|(s, m)| *s == status && m == method) {
            arrived.push((status, method.to_string()));
        }
    }
    arrived
}

#[cfg(test)]
mod tests;
