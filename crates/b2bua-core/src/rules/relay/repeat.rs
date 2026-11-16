//! The responses a transaction layer hands the TU again: a 2xx to an INVITE
//! is repeated end-to-end until ACKed (RFC 3261 §13.3.1.4), so it reaches the
//! rules more than once. This predicate is the one reading of "already
//! handled" the rule that re-ACKs and the message ring share; a reliable
//! provisional's repeat is the engine's to drop before the rules. Each copy is
//! counted on the INVITE's record, and only the copies a compliant answerer
//! sends are owed a re-ACK.

use b2bua_sdk::event::CallEvent;
use b2bua_sdk::model::{RuleAction, RuleContext};
use call::{Call, Dialog, DialogId, InviteExchange, Leg};
use sip_message::{Method, SipMessage, SipResponse};
use sip_retransmit::Class;

/// Whether `resp` is a copy of an INVITE 2xx `leg`'s `dialog` already took
/// (RFC 3261 §13.3.1.4 — the answerer repeats it until ACKed): the dialog
/// holds the taking's mark — its ACK client transaction, or the ACK it still
/// awaits from the peer while a delayed-offer answer is composable by no one
/// else — the response carries the dialog's remote tag (every fork of one
/// INVITE answers on that INVITE's CSeq, §12.1.2, so the CSeq alone would take
/// a losing fork's late 2xx for a repeat) and echoes the CSeq of the INVITE
/// last sent on it while no pending relay of that CSeq is open (a first-time
/// re-INVITE final is claimed by the relay rules). A 2xx answering anything
/// but an INVITE — the 200 to a CANCEL carries the INVITE's number — is never
/// a copy of one, nor is a retired fork's: its release answers each copy.
pub(crate) fn retransmitted_2xx(leg: &Leg, dialog: &Dialog, resp: &SipResponse) -> bool {
    let taken = call::helpers::invite_taken(dialog) && !dialog.is_retired();
    if !taken || !(200..300).contains(&resp.status()) || resp.cseq().method() != Method::Invite {
        return false;
    }
    if resp.to().tag().unwrap_or_default() != dialog.peer_tag {
        return false;
    }
    let cseq = resp.cseq().seq();
    super::acked_invite_cseq(leg, dialog) == Some(cseq)
        && call::helpers::find_relay(dialog, i64::from(cseq)).is_none()
}

/// Count `event`, received on `leg_id`'s dialog `named`, on that dialog's
/// INVITE record when it is a copy of the 2xx the dialog took
/// ([`retransmitted_2xx`]). Returns whether the copy is one past what a
/// compliant answerer sends ([`re_ack_owed`] false).
pub(crate) fn count_copy(
    call: &mut Call,
    leg_id: &str,
    named: Option<&DialogId>,
    event: &CallEvent,
) -> bool {
    let CallEvent::Sip { message, .. } = event else { return false };
    let SipMessage::Response(resp) = message.as_ref() else { return false };
    let Some(id) = named else { return false };
    let copy = call::helpers::find_leg(call, leg_id).is_some_and(|leg| {
        leg.dialogs
            .by_tags(&id.our_tag, &id.peer_tag)
            .is_some_and(|d| retransmitted_2xx(leg, d, resp))
    });
    if !copy {
        return false;
    }
    let mut past = false;
    call::helpers::update_dialog(call, leg_id, id, |d| {
        if let Some(x) = call::helpers::sent_exchange_mut(d) {
            x.copies_received = x.copies_received.saturating_add(1);
            past = !re_ack_owed(x);
        }
    });
    past
}

/// What the copy of a taken 2xx `ctx` carries draws: the re-ACK of its leg
/// while [`re_ack_owed`], nothing past it.
pub(crate) fn re_ack(ctx: &RuleContext) -> Vec<RuleAction> {
    let owed = ctx.dialog().and_then(call::helpers::sent_exchange).is_some_and(re_ack_owed);
    if !owed {
        return Vec::new();
    }
    vec![RuleAction::AckLeg { leg_id: ctx.source_leg_id.to_string(), body: None }]
}

/// Whether the copy of a taken 2xx that `x`, its INVITE's exchange, counted
/// last is owed its re-ACK (RFC 3261 §13.2.2.4): a compliant answerer re-sends
/// a 2xx on the T1-doubling, T2-capped ladder and gives up at 64·T1
/// (§13.3.1.4), so a copy past that ladder's re-sends is no retransmission and
/// is absorbed; an answerer re-sending on every ACK stops there.
fn re_ack_owed(x: &InviteExchange) -> bool {
    x.copies_received <= Class::Final2xx.resends()
}
