//! A re-INVITE toward the originator that waits in a service's state for an
//! ACK awaited on her dialog (RFC 3261 §14.1, ADR-0064 §3): when it waits,
//! which ACK releases it, what it keeps of the message whose description it
//! offers, and the lines it reads back when it leaves.

use b2bua_sdk::model::RuleContext;
use call::helpers::InviteInProgress;
use call::{PendingResync, RequestOrigin};
use sip_message::{SipHeader, SipStr};

/// Whether an ACK still to come holds an INVITE transaction in progress on
/// A's dialog, so no re-INVITE of this stack's may leave there until it
/// arrives (RFC 3261 §14.1): her ACK of a 2xx this stack sent her, or her
/// peer's ACK of the 2xx she gave the re-INVITE relayed to her, which this
/// stack relays on.
pub(crate) fn ack_awaited_on_her_dialog(ctx: &RuleContext) -> bool {
    match ctx.call.invite_in_progress("a") {
        Some(InviteInProgress::AnsweredUnacknowledged { .. }) => true,
        Some(InviteInProgress::TakenUnacknowledged { .. }) => {
            call::helpers::carrying_dialog(ctx.call.a_leg())
                .and_then(|d| d.sent_invite.as_ref())
                .is_some_and(|sent| sent.origin == RequestOrigin::Relayed)
        }
        _ => false,
    }
}

/// Whether `ctx`'s ACK frees A's dialog of the ACK it awaits
/// ([`ack_awaited_on_her_dialog`]) once relayed to its peer: her own, which
/// discharged the 2xx this stack sent her before any rule ran, or her
/// peer's, owed on her dialog ([`call::helpers::relayed_ack_owed`]) and
/// discharged by its relay to her this turn.
pub(crate) fn ack_frees_her_dialog(ctx: &RuleContext) -> bool {
    let in_progress = ctx.call.invite_in_progress("a");
    if ctx.source_leg_id == "a" {
        return in_progress.is_none();
    }
    let from_her_peer = ctx.call.active_peer().is_some_and(|p| p.leg_b == ctx.source_leg_id);
    let owed = ctx.request().is_some_and(|r| {
        call::helpers::relayed_ack_owed(ctx.call.a_leg(), i64::from(r.cseq().seq()))
    });
    from_her_peer
        && matches!(in_progress, Some(InviteInProgress::TakenUnacknowledged { .. }))
        && owed
}

/// The re-INVITE offering `body`, written by the peer of `leg_id`, with the
/// lines of `headers` that describe it or state its sender's advertisement.
pub(crate) fn park(leg_id: &str, body: &[u8], headers: &[SipHeader]) -> PendingResync {
    let lines = sip_message::generators::body_and_advertisement_lines(headers)
        .into_iter()
        .map(|h| call::SipHeader { name: h.name.to_string(), value: h.value.to_string() })
        .collect();
    PendingResync { leg_id: leg_id.to_string(), body: body.to_vec(), lines }
}

/// The lines `pending` kept, as header lines of the message they came from.
pub(crate) fn lines(pending: &PendingResync) -> Vec<SipHeader> {
    pending
        .lines
        .iter()
        .map(|l| SipHeader { name: SipStr::owned(&l.name), value: SipStr::owned(&l.value) })
        .collect()
}
