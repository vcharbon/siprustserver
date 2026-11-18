//! The responses a transaction layer hands the TU again: a 2xx to an INVITE
//! is repeated end-to-end until ACKed (RFC 3261 §13.3.1.4), so it reaches the
//! rules more than once, and so does a non-2xx of a round the record never
//! held, whose ACK this stack sent itself and no transaction re-sends
//! (§17.1.1.3). [`copy_of_taken_2xx`] and [`copy_of_earlier_2xx`] (a 2xx to
//! an INVITE the dialog sent another after) are the readings of "already
//! handled" the rules that re-ACK and the message ring share; a reliable provisional's
//! repeat is the engine's to drop before the rules. Each copy is counted on the
//! INVITE's record, and only the copies a compliant answerer sends are owed a
//! re-ACK (ADR-0057 §10): the action that re-sends the retained ACK reads the
//! mark ([`re_ack_owed`]).

use b2bua_sdk::event::CallEvent;
use b2bua_sdk::model::{RuleAction, RuleContext};
use call::{AckAhead, Call, Dialog, DialogId, InviteExchange, Leg, ReAckedCopy};
use sip_message::{Method, SipMessage, SipResponse};
use sip_retransmit::Class;

use crate::copy_schedule;

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
/// a copy of one, nor is a retired fork's: its release re-ACKs a copy while a
/// re-ACK is owed ([`re_ack_owed`]).
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

/// Whether `resp` is a copy of the 2xx to an INVITE `dialog` sent before its
/// last (§13.3.1.4): a 2xx to an INVITE under the dialog's remote tag echoing
/// a CSeq below that last INVITE's, other than the latest INVITE whose round
/// ended with no 2xx taken ([`Dialog::untaken_invite`]): that 2xx is its
/// first. An earlier untaken INVITE is read as taken: the record keeps no
/// history of it. Every INVITE before the last was settled before it left
/// (§14.1), so the copy answers nothing pending; only the one whose ACK the
/// last INVITE carries ahead ([`copy_of_2xx_ahead`]) is re-ACKed.
pub(crate) fn copy_of_earlier_2xx(dialog: &Dialog, resp: &SipResponse) -> bool {
    let Some(last) = dialog.sent_invite.as_ref() else { return false };
    let cseq = i64::from(resp.cseq().seq());
    invite_2xx_on(dialog, resp) && cseq < last.outbound_cseq && dialog.untaken_invite != Some(cseq)
}

/// Whether `resp`, a [`copy_of_earlier_2xx`] on the dialog whose last INVITE's
/// exchange is `x`, is a copy of the 2xx whose ACK `x` carries ahead
/// ([`InviteExchange::ack_ahead`]): it echoes that ACK's CSeq.
pub(crate) fn copy_of_2xx_ahead(x: &InviteExchange, resp: &SipResponse) -> bool {
    x.ack_ahead.as_ref().is_some_and(|ahead| ahead.invite_cseq == i64::from(resp.cseq().seq()))
}

/// Whether `resp` is a copy of a non-2xx INVITE final `leg`'s `dialog` ACKed
/// on this stack's own account (§17.1.1.3), in a round the record never held:
/// the dialog's INVITE record holds no image of the INVITE and keeps the ACK
/// it sent, which names `resp`'s CSeq.
pub(crate) fn copy_of_acked_failure(leg: &Leg, dialog: &Dialog, resp: &SipResponse) -> bool {
    resp.status() >= 300
        && resp.cseq().method() == Method::Invite
        && call::helpers::sent_invite_image(leg, dialog).is_none()
        && call::helpers::sent_exchange(dialog).is_some_and(|x| x.emitted_ack.is_some())
        && super::acked_invite_cseq(leg, dialog) == Some(resp.cseq().seq())
}

/// Whether `resp` is a 2xx to an INVITE under `dialog`'s remote tag.
fn invite_2xx_on(dialog: &Dialog, resp: &SipResponse) -> bool {
    (200..300).contains(&resp.status())
        && resp.cseq().method() == Method::Invite
        && resp.to().tag().unwrap_or_default() == dialog.peer_tag
}

/// A copy [`count_copy`] counted on its INVITE's record, whose retained ACK
/// alone answers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CountedCopy {
    /// Owed its re-ACK.
    Owed,
    /// Past the answerer's ladder: absorbed, counted under the ladder's class.
    Absorbed(Class),
}

/// Which exchange's record a copy is counted on.
#[derive(Clone, Copy)]
enum CopyOf {
    /// The dialog's last INVITE: a [`copy_of_taken_2xx`] or a
    /// [`copy_of_acked_failure`].
    Last,
    /// The INVITE before it, whose ACK the last carries ahead.
    Ahead,
}

/// Count `event`, received at `now_ms` on `leg_id`'s dialog `named`, before
/// the rules read it, and mark it owed its re-ACK or not: the re-ACK reads the
/// mark ([`re_ack_owed`], [`re_ack_ahead_owed`]). A [`copy_of_taken_2xx`] or a
/// [`copy_of_acked_failure`] counts on the dialog's INVITE record, a
/// [`copy_of_2xx_ahead`] on the ACK that record carries ahead. Every copy the
/// answerer sends counts toward its ladder (§13.3.1.4, §17.2.1), also one no
/// rule re-ACKs: a copy arriving before this side's ACK left (no `ack_branch`
/// yet), or one on a retired a-leg dialog (a fork release is a b-leg's). Any
/// other [`copy_of_earlier_2xx`] is absorbed: the record holds no ACK of it.
/// `None` when `event` is no such copy.
pub(crate) fn count_copy(
    call: &mut Call,
    leg_id: &str,
    named: Option<&DialogId>,
    event: &CallEvent,
    now_ms: i64,
) -> Option<CountedCopy> {
    let CallEvent::Sip { message, .. } = event else { return None };
    let SipMessage::Response(resp) = message.as_ref() else { return None };
    let id = named?;
    let leg = call::helpers::find_leg(call, leg_id)?;
    let d = leg.dialogs.by_tags(&id.our_tag, &id.peer_tag)?;
    let class = copy_schedule::class_of(resp.status());
    let copy = if copy_of_taken_2xx(leg, d, resp) || copy_of_acked_failure(leg, d, resp) {
        CopyOf::Last
    } else if !copy_of_earlier_2xx(d, resp) {
        return None;
    } else if call::helpers::sent_exchange(d).is_some_and(|x| copy_of_2xx_ahead(x, resp)) {
        CopyOf::Ahead
    } else {
        return Some(CountedCopy::Absorbed(class));
    };
    let mut counted = None;
    call::helpers::update_dialog(call, leg_id, id, |d| {
        let Some(x) = call::helpers::sent_exchange_mut(d) else { return };
        let (received, re_acked) = match copy {
            CopyOf::Last => (&mut x.copies_received, &mut x.re_acked_copy),
            CopyOf::Ahead => match x.ack_ahead.as_mut() {
                Some(ahead) => (&mut ahead.copies_received, &mut ahead.re_acked_copy),
                None => return,
            },
        };
        counted = Some(count_on(received, re_acked, class, now_ms));
    });
    counted
}

/// Count one more copy of a final of `class` on `received`, arrived at
/// `now_ms`, marking it `re_acked` when owed its re-ACK
/// ([`copy_schedule::owed`]).
fn count_on(
    received: &mut u32,
    re_acked: &mut Option<ReAckedCopy>,
    class: Class,
    now_ms: i64,
) -> CountedCopy {
    *received = received.saturating_add(1);
    let since = re_acked.map(|last| now_ms.saturating_sub(last.at_ms));
    if copy_schedule::owed(class, *received, since) {
        *re_acked = Some(ReAckedCopy { copy: *received, at_ms: now_ms });
        CountedCopy::Owed
    } else {
        CountedCopy::Absorbed(class)
    }
}

/// What the copy of a final `ctx` carries asks of its leg: the retained ACK,
/// which the action re-sends while the copy is owed it ([`re_ack_owed`]).
pub(crate) fn re_ack(ctx: &RuleContext) -> Vec<RuleAction> {
    vec![RuleAction::AckLeg { leg_id: ctx.source_leg_id.to_string(), body: None }]
}

/// Whether the copy of a final whose ACK `x`, its INVITE's exchange, retains,
/// counted last, is owed its re-ACK: [`count_copy`] marked it so. No copy
/// counted is owed nothing.
pub(crate) fn re_ack_owed(x: &InviteExchange) -> bool {
    x.re_acked_copy.is_some_and(|last| last.copy == x.copies_received)
}

/// [`re_ack_owed`] for the ACK `ahead` the dialog's last INVITE carries ahead:
/// the copy of its 2xx [`count_copy`] counted last is owed it.
pub(crate) fn re_ack_ahead_owed(ahead: &AckAhead) -> bool {
    ahead.re_acked_copy.is_some_and(|last| last.copy == ahead.copies_received)
}
