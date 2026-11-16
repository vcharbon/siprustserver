//! The responses a transaction layer hands the TU again: a 2xx to an INVITE
//! is repeated end-to-end until ACKed (RFC 3261 §13.3.1.4), so it reaches the
//! rules more than once. [`copy_of_taken_2xx`] is the one reading of "already
//! handled" the rules that re-ACK and the message ring share; a reliable
//! provisional's repeat is the engine's to drop before the rules. Each copy is
//! counted on the INVITE's record, and only the copies a compliant answerer
//! sends are owed a re-ACK.

use b2bua_sdk::event::CallEvent;
use b2bua_sdk::model::{RuleAction, RuleContext};
use call::{Call, Dialog, DialogId, InviteExchange, Leg, ReAckedCopy};
use sip_message::{Method, SipMessage, SipResponse};
use sip_retransmit::timers::T1;
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
    if !taken || !invite_2xx_on(dialog, resp) {
        return false;
    }
    let cseq = resp.cseq().seq();
    super::acked_invite_cseq(leg, dialog) == Some(cseq)
        && call::helpers::find_relay(dialog, i64::from(cseq)).is_none()
}

/// Whether `resp` is a copy of a 2xx `leg`'s `dialog` took: a
/// [`retransmitted_2xx`], or a copy of the 2xx of a fork straggler already
/// released (§13.2.2.4) — the dialog is retired, holds the ACK that 2xx drew,
/// and `resp` carries its tag and that ACK's CSeq.
pub(crate) fn copy_of_taken_2xx(leg: &Leg, dialog: &Dialog, resp: &SipResponse) -> bool {
    let released = dialog.is_retired()
        && call::helpers::sent_exchange(dialog).is_some_and(|x| x.emitted_ack.is_some())
        && invite_2xx_on(dialog, resp)
        && super::acked_invite_cseq(leg, dialog) == Some(resp.cseq().seq());
    released || retransmitted_2xx(leg, dialog, resp)
}

/// Whether `resp` is a 2xx to an INVITE under `dialog`'s remote tag.
fn invite_2xx_on(dialog: &Dialog, resp: &SipResponse) -> bool {
    (200..300).contains(&resp.status())
        && resp.cseq().method() == Method::Invite
        && resp.to().tag().unwrap_or_default() == dialog.peer_tag
}

/// Count `event`, received at `now_ms` on `leg_id`'s dialog `named`, on that
/// dialog's INVITE record when it is a [`copy_of_taken_2xx`], before the rules
/// read it, and mark it owed its re-ACK or not: the re-ACK reads the mark
/// ([`re_ack_owed`]). Returns whether the copy is absorbed.
pub(crate) fn count_copy(
    call: &mut Call,
    leg_id: &str,
    named: Option<&DialogId>,
    event: &CallEvent,
    now_ms: i64,
) -> bool {
    let CallEvent::Sip { message, .. } = event else { return false };
    let SipMessage::Response(resp) = message.as_ref() else { return false };
    let Some(id) = named else { return false };
    let copy = call::helpers::find_leg(call, leg_id).is_some_and(|leg| {
        leg.dialogs
            .by_tags(&id.our_tag, &id.peer_tag)
            .is_some_and(|d| copy_of_taken_2xx(leg, d, resp))
    });
    if !copy {
        return false;
    }
    let mut absorbed = false;
    call::helpers::update_dialog(call, leg_id, id, |d| {
        if let Some(x) = call::helpers::sent_exchange_mut(d) {
            x.copies_received = x.copies_received.saturating_add(1);
            if owes_re_ack(x, now_ms) {
                x.re_acked_copy = Some(ReAckedCopy { copy: x.copies_received, at_ms: now_ms });
            } else {
                absorbed = true;
            }
        }
    });
    absorbed
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
/// last is owed its re-ACK: [`count_copy`] marked it so.
pub(crate) fn re_ack_owed(x: &InviteExchange) -> bool {
    x.re_acked_copy.is_some_and(|last| last.copy == x.copies_received)
}

/// Whether the copy `x` just counted, arriving at `now_ms`, is owed its re-ACK
/// (RFC 3261 §13.2.2.4): the re-sends of the T1-doubling, T2-capped ladder
/// (§13.3.1.4) are owed at any pace (a duplicate, one crossing the ACK); a
/// later copy only T1 after the last re-ACK, as a peer's larger T1 spaces them
/// (§17.1.1.1). A peer answering each ACK with the 2xx again stops there.
fn owes_re_ack(x: &InviteExchange, now_ms: i64) -> bool {
    x.copies_received <= Class::Final2xx.resends()
        || x.re_acked_copy.is_none_or(|last| {
            u64::try_from(now_ms.saturating_sub(last.at_ms)).is_ok_and(|since| since >= T1)
        })
}
