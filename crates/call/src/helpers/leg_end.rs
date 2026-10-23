//! The facts a leg's end records (RFC 3261 §9.1, §12.1, §15): the final its
//! INVITE took, this stack's CANCEL of it, and the early dialogs those end or
//! retire. The leg's state reads off them ([`crate::model::Leg::state`]).

use crate::model::{
    Call, DialogEnd, DialogState, EndingBy, Leg, MessageDirection, MessageEntry, RetiredDialog,
};

use super::lens::update_leg;

/// Retire every early dialog of `leg`, its open relays kept for their finals
/// (RFC 3261 §8.1.3.3).
pub fn retire_early_dialogs(leg: &mut Leg) {
    for d in leg.dialogs.iter_mut().filter(|d| d.state == DialogState::Early) {
        let open_relays = std::mem::take(&mut d.inbound_pending_requests);
        d.state = DialogState::Retired(RetiredDialog { open_relays, release: None });
    }
}

/// Note the CANCEL of `leg_id`'s INVITE leaving (RFC 3261 §9.1): each early
/// dialog that INVITE opened is ending, and ends on the INVITE's final.
pub fn cancel_early_dialogs(call: Call, leg_id: &str) -> Call {
    update_leg(call, leg_id, |l| {
        for d in l.dialogs.iter_mut().filter(|d| d.state == DialogState::Early) {
            d.state = DialogState::Ending(EndingBy::Cancel);
        }
    })
}

/// Record the final sent on a leg's initial inbound INVITE
/// ([`Leg::invite_final_sent`]). The first final stands: a leg that already
/// carries one is left as is — the transaction took exactly that one final.
/// A non-2xx final ends every early dialog of the leg (RFC 3261 §12.1).
pub fn record_invite_final(call: Call, leg_id: &str, status: u16) -> Call {
    update_leg(call, leg_id, |l| {
        if l.invite_final_sent.is_none() && status >= 300 {
            reject_early_dialogs(l, status);
        }
        l.invite_final_sent.get_or_insert(status);
    })
}

/// Record the non-2xx final `status` that answered, on `leg_id`, the INVITE
/// of CSeq `cseq` this stack sent: when it is the INVITE that dialled the leg
/// ([`Leg::outbound_invite`]), every early dialog it created ends (RFC 3261
/// §12.1). A final to a re-INVITE changes no dialog's state (§14.1).
pub fn record_dialling_invite_rejected(call: Call, leg_id: &str, cseq: i64, status: u16) -> Call {
    update_leg(call, leg_id, |l| {
        if l.outbound_invite.as_ref().is_some_and(|i| i.cseq == cseq) {
            reject_early_dialogs(l, status);
        }
    })
}

/// Record the final `status` that answered, on `leg_id`, the INVITE of CSeq
/// `cseq` this stack sent, when it is the INVITE that dialled the leg
/// ([`Leg::outbound_invite`]): the leg's INVITE outcome, the first final
/// standing (RFC 3261 §17.1.1.2); a non-2xx final ends every early dialog it
/// created (§12.1).
pub fn record_dialled_invite_final(call: Call, leg_id: &str, cseq: i64, status: u16) -> Call {
    update_leg(call, leg_id, |l| {
        let Some(invite) = l.outbound_invite.as_mut().filter(|i| i.cseq == cseq) else { return };
        invite.record_final(status);
        if status >= 300 {
            reject_early_dialogs(l, status);
        }
    })
}

/// Give up whatever `leg` still awaits on its own clock: an INVITE without a
/// final takes the 408 its transaction's end would have given it (RFC 3261
/// §8.1.3.1), the dialogs it opened retiring with their open relays; a dialog
/// a 2xx confirmed, its BYE in flight or not, ends without a final
/// (§15.1.1).
pub fn abandon_leg(leg: &mut Leg) {
    if let Some(invite) = leg.outbound_invite.as_mut().filter(|i| i.final_received.is_none()) {
        invite.record_final(408);
        for d in leg.dialogs.iter_mut().filter(|d| d.awaits_invite_final()) {
            let open_relays = std::mem::take(&mut d.inbound_pending_requests);
            d.state = DialogState::Retired(RetiredDialog { open_relays, release: None });
        }
    }
    for d in leg.dialogs.iter_mut().filter(|d| d.is_confirmed()) {
        d.state = DialogState::Ended(DialogEnd::OurBye { final_status: None });
    }
}

/// Note this stack's CANCEL of `leg_id`'s INVITE, stating `stated`, leaving
/// (RFC 3261 §9.1): the leg's INVITE is cancelled, and each early dialog it
/// opened is ending, to end on the INVITE's final.
pub fn record_cancel_sent(call: Call, leg_id: &str, stated: Vec<(String, String)>) -> Call {
    let call = update_leg(call, leg_id, |l| {
        if let Some(invite) = l.outbound_invite.as_mut() {
            invite.cancel_sent = true;
            invite.cancel_headers = stated;
        }
    });
    cancel_early_dialogs(call, leg_id)
}

/// End every early dialog of `leg` — its CANCEL in flight or not — under
/// the non-2xx final `status` its dialog-creating INVITE took (RFC 3261 §12.1).
pub fn reject_early_dialogs(leg: &mut Leg, status: u16) {
    for d in leg.dialogs.iter_mut().filter(|d| d.awaits_invite_final()) {
        d.state = DialogState::Ended(DialogEnd::Rejected(status));
    }
}

/// Record the `487` the transaction layer answered a CANCEL of `leg_id`'s
/// initial INVITE with (RFC 3261 §9.2): the wire's final, over a 2xx built
/// too late ([`Leg::answer_preempted`]), whose ring entry and ACK obligation
/// go; every dialog the INVITE opened ends under the 487 (§12.1).
pub fn record_cancelled_invite(call: Call, leg_id: &str) -> Call {
    update_leg(call, leg_id, |l| {
        if let Some(answer) = l.invite_final_sent.filter(|s| (200..300).contains(s)) {
            l.answer_preempted = Some(answer);
            l.messages.entries.retain(|e| !is_sent_invite_2xx(e));
            for d in l.dialogs.iter_mut() {
                d.answered_2xx = None;
                d.awaited_ack_cseq = None;
                if d.state == DialogState::Confirmed {
                    d.state = DialogState::Early;
                }
            }
        }
        reject_early_dialogs(l, 487);
        l.invite_final_sent = Some(487);
    })
}

/// A 2xx to an INVITE the stack sent on a leg. Before the leg's initial
/// INVITE is answered on the wire it is that INVITE's answer: no other INVITE
/// can be in progress on an unconfirmed leg.
fn is_sent_invite_2xx(e: &MessageEntry) -> bool {
    e.direction != MessageDirection::Received
        && e.method == "INVITE"
        && e.code.is_some_and(|c| (200..300).contains(&c))
}
