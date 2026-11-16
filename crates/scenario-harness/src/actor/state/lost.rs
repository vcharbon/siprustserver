//! What a leg's observed state reads of the datagrams the call's loss model
//! dropped ([`super::Observation::Lost`]), each kept with how many responses
//! the leg had observed when it was folded in: the drop came before every
//! later response.

use super::{LegObservation, ResponseFact, StateInner};
use crate::realcall::LostMessage;

/// One datagram the loss model dropped, to or from a leg.
#[derive(Debug, Clone)]
pub(super) struct Lost {
    /// How many responses the leg had observed when the drop was folded in.
    responses_before: usize,
    message: LostMessage,
}

impl LegObservation {
    /// Fold the drop of `message`, after the responses observed so far.
    pub(super) fn note_lost(&mut self, message: LostMessage) {
        self.lost.push(Lost { responses_before: self.responses.len(), message });
    }

    /// The datagrams to or from this leg the call's loss model dropped, in
    /// the order dropped.
    pub fn lost(&self) -> impl Iterator<Item = &LostMessage> {
        self.lost.iter().map(|l| &l.message)
    }

    /// Whether the loss model dropped a 2xx to this leg's INVITE on its way
    /// to the leg before the leg observed its response at `index`
    /// ([`responses`](Self::responses)): the sender had answered before
    /// that response arrived.
    pub fn lost_answer_before(&self, index: usize) -> bool {
        self.lost.iter().any(|l| {
            l.responses_before <= index
                && l.message.inbound
                && l.message.is_invite_response_in(200..300)
        })
    }

    /// Whether the loss model dropped a provisional above 100 to this leg's
    /// INVITE on its way to the leg before the leg received one: the early
    /// dialog it opened may never be known to the leg. Once true, it stays.
    pub fn ring_lost_unheard(&self) -> bool {
        let ring = |f: &ResponseFact| (101..200).contains(&f.status) && f.cseq_method == "INVITE";
        let first_ring = self.responses.iter().position(ring).unwrap_or(usize::MAX);
        self.lost.iter().any(|l| {
            l.responses_before <= first_ring
                && l.message.inbound
                && l.message.is_invite_response_in(101..200)
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
