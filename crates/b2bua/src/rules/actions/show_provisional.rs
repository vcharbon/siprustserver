//! Showing a reliable provisional to the party its INVITE came from under
//! this stack's own number (RFC 3262 §3, §4): the face's sequence, and the
//! tag of the dialog it is shown in.

use call::{CSeq, Call, PendingRequest, RSeq};
use sip_message::header::{HeaderName, HeaderValue, To};
use sip_message::{SipHeader, SipStr};

use crate::rules::relay;

use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Restate a relayed reliable provisional's `RSeq` with the shown dialog's
    /// own number (RFC 3262 §4, errata 4603) and record it shown there, so the
    /// PRACK naming it translates back onto the responder's sequence.
    /// `shown_tag` is this stack's tag on the dialog the provisional is
    /// relayed into and `shown_invite_cseq` the CSeq of the INVITE it answers
    /// on that face — the two the PRACK's `RAck` names beside the number
    /// (§7.2). A provisional relayed without an `RSeq` (a masking policy
    /// stripped it) is not a reliable one and takes no number; one the engine
    /// did not record, or one toward a party whose request offered no
    /// `100rel` (`admitted` false, §3), leaves unreliably and is shown to no
    /// one. `carried_sdp`: the provisional carries a description toward that
    /// face. Returns the number taken, so the emission arms its §3 ladder.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn own_relayed_rseq(
        &mut self,
        call: &mut Call,
        admitted: bool,
        shown_tag: &str,
        shown_invite_cseq: i64,
        source_leg_id: &str,
        resp: &sip_message::SipResponse,
        headers: &mut Vec<SipHeader>,
        carried_sdp: bool,
    ) -> Option<RSeq> {
        if !headers.iter().any(|h| HeaderName::RSeq.matches(&h.name)) {
            return None;
        }
        let recorded = super::provisional_arrival::recorded(call, source_leg_id, resp);
        let Some(id) = recorded.filter(|_| admitted) else {
            relay::strip_reliability(headers);
            return None;
        };
        let initial = if call::helpers::starts_shown_sequence(call, shown_tag) {
            RSeq(self.turn.ids.new_sequence_number())
        } else {
            RSeq(0)
        };
        let shown = call::helpers::show_provisional(
            call,
            &id,
            shown_tag,
            CSeq::of_i64(shown_invite_cseq),
            carried_sdp,
            initial,
        )?;
        relay::own_the_rseq(headers, shown);
        Some(shown)
    }
}

/// The tag this stack showed the originator of `pending` — its own on the
/// dialog the response is relayed into, read off the originator's `To` (RFC
/// 3261 §12.2.1.1), which the relayed response echoes verbatim and the
/// originator's PRACK names again. `None` when that `To` names no tag.
pub(super) fn shown_tag_of(pending: &PendingRequest) -> Option<String> {
    To::parse(&SipStr::owned(&pending.source_to)).ok().and_then(|to| to.tag().map(str::to_string))
}
