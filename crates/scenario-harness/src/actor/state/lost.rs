//! What a leg's observed state reads of the datagrams the call's loss model
//! dropped ([`super::Observation::Lost`]), each ordered against the responses
//! by the network's arrival order it carries
//! ([`LostMessage::arrived_before`]), never by when it was folded in: the
//! drop is noted as the network drops it, a response only once the leg reads
//! it.

use super::{LegObservation, StateInner};
use crate::realcall::LostMessage;

impl LegObservation {
    /// Fold the drop of `message`.
    pub(super) fn note_lost(&mut self, message: LostMessage) {
        self.lost.push(message);
    }

    /// The datagrams to or from this leg the call's loss model dropped, in
    /// the order dropped.
    pub fn lost(&self) -> impl Iterator<Item = &LostMessage> {
        self.lost.iter()
    }

    /// Whether the loss model dropped a 2xx to this leg's INVITE on its way
    /// to the leg before any final to its CANCEL had arrived: the sender had
    /// answered before it settled the CANCEL.
    pub fn lost_answer_before_cancel_final(&self) -> bool {
        self.lost.iter().any(|m| {
            m.inbound
                && m.is_invite_response_in(200..300)
                && !m.after_response_in("CANCEL", 200..700)
        })
    }

    /// Whether the loss model dropped a provisional above 100 to this leg's
    /// INVITE on its way to the leg before one had arrived: the early dialog
    /// it opened may never be known to the leg. Once true, it stays.
    pub fn ring_lost_unheard(&self) -> bool {
        self.lost.iter().any(|m| {
            m.inbound
                && m.is_invite_response_in(101..200)
                && !m.after_response_in("INVITE", 101..200)
        })
    }

    /// Whether the loss model dropped a provisional with a session
    /// description to this leg's INVITE on its way to the leg: early media
    /// the leg was sent and never received.
    pub fn lost_early_media(&self) -> bool {
        self.lost().any(|m| m.inbound && m.is_invite_response_in(101..200) && m.sdp)
    }
}

impl StateInner {
    /// Whether `role` received a provisional above 100 to an INVITE, or the
    /// call's loss model dropped one on any leg, so her ringing may never
    /// come: the gate of a step timed from the caller's ringing.
    pub fn rang_or_ring_lost(&self, role: &str) -> bool {
        self.leg_rang(role)
            || self
                .legs
                .values()
                .flat_map(LegObservation::lost)
                .any(|m| m.is_invite_response_in(101..200))
    }
}
