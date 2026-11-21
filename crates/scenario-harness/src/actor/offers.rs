//! Where the offer of a dialog-creating INVITE rides, as one endpoint tracks
//! it (RFC 3261 §13.2.1, RFC 3262 §5).
//!
//! An INVITE that carries a description makes the offer, and each early
//! dialog's first reliable provisional or its 2xx carries the answer. An
//! INVITE that carries none leaves the offer to the callee: per early dialog,
//! the first reliable provisional carries it and the PRACK the answer, or, when
//! no reliable provisional did, the 2xx carries it and the ACK the answer. No
//! other provisional of that dialog carries a description, and the 2xx after
//! an early offer carries none (RFC 6337 §3.1).

use std::collections::HashSet;

use sip_message::SipResponse;

/// One endpoint's reading of its INVITE transaction's offer.
#[derive(Debug, Default)]
pub(super) struct Offers {
    /// Caller: the INVITE it originated carried a description.
    pub(super) invite_offered: bool,
    /// The early dialogs whose offer a reliable provisional carried: by the
    /// callee's tag on both sides, the caller's PRACK having answered it.
    early: HashSet<String>,
}

impl Offers {
    /// Forget the early dialogs of a previous INVITE.
    pub(super) fn reset(&mut self) {
        self.early.clear();
    }

    /// Caller: whether the PRACK to the reliable provisional `resp` answers
    /// its offer. Once per early dialog.
    pub(super) fn prack_answers(&mut self, resp: &SipResponse) -> bool {
        !self.invite_offered && resp.sdp().is_some() && self.early.insert(tag_of(resp))
    }

    /// Caller: whether the ACK to the 2xx `resp` answers its offer.
    pub(super) fn ack_answers(&self, resp: &SipResponse) -> bool {
        !self.invite_offered && resp.sdp().is_some() && !self.early.contains(&tag_of(resp))
    }

    /// Callee: whether a provisional on the early dialog `tag` carries a
    /// description. With the offer in the INVITE, `wanted` decides; without,
    /// only the dialog's first reliable one does, and it is the offer.
    pub(super) fn provisional_carries(
        &mut self,
        invite_offered: bool,
        tag: &str,
        reliable: bool,
        wanted: bool,
    ) -> bool {
        if invite_offered {
            return wanted;
        }
        reliable && self.early.insert(tag.to_string())
    }

    /// Callee: whether the 2xx on the dialog `tag` carries a description: the
    /// answer to the INVITE's offer, or the offer no reliable provisional of
    /// that dialog made.
    pub(super) fn final_carries(&self, invite_offered: bool, tag: &str) -> bool {
        invite_offered || !self.early.contains(tag)
    }
}

/// The callee's tag a response names: its early dialog.
fn tag_of(resp: &SipResponse) -> String {
    resp.to().tag().unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delayed_offer_rides_one_reliable_provisional_per_dialog() {
        let mut o = Offers::default();
        assert!(!o.provisional_carries(false, "a", false, true), "unreliable carries none");
        assert!(o.provisional_carries(false, "a", true, false), "first reliable is the offer");
        assert!(!o.provisional_carries(false, "a", true, true), "a later one carries none");
        assert!(o.provisional_carries(false, "b", true, false), "each dialog its own");
        assert!(!o.final_carries(false, "a"));
        assert!(o.final_carries(false, "c"), "a dialog with no early offer: the 2xx offers");
    }

    #[test]
    fn an_offer_in_the_invite_leaves_the_draw_alone() {
        let mut o = Offers::default();
        assert!(o.provisional_carries(true, "a", false, true));
        assert!(!o.provisional_carries(true, "a", true, false));
        assert!(o.final_carries(true, "a"));
    }
}
