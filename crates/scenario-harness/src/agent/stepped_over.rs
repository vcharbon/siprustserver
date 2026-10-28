//! What a scripted receive takes without surfacing it, whatever it waits
//! for: the hop ACK of an armed obligation, and a repeat of an ACK or of a
//! provisional this UA already received.

use sip_message::{SipRequest, SipResponse};

use super::Agent;

impl Agent {
    /// Whether `r` is the hop ACK of an armed §17.1.1.3 obligation on this UA,
    /// marking it fulfilled (idempotent), or a repeat of an ACK it received
    /// (§13.3.1.4). A receive path that would otherwise ERROR on an unexpected
    /// ACK calls this and absorbs instead: neither trips a body.
    pub(crate) fn ack_obligation_claims(&self, r: &SipRequest) -> bool {
        self.txn.hop_ack_claims(r) || self.txn.repeats_an_ack(r)
    }

    /// Whether `r` is a provisional byte-identical to one this UA already
    /// received (§13.2.2.1): a scripted wait for any other message steps over
    /// it.
    pub(crate) fn repeated_provisional(&self, r: &SipResponse) -> bool {
        self.txn.repeats_a_provisional(r)
    }
}
