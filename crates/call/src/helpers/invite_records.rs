//! The INVITE request records of a call (ADR-0055 §2): the INVITE this side
//! last sent on a dialog and the ACK its 2xx owes (RFC 3261 §13.2.2.4), the
//! INVITE it answered 2xx on a dialog and the ACK it awaits (§13.3.1.4), and
//! the image of a leg's dialling INVITE, which every dialog it created reads.

use crate::model::{
    Call, Dialog, DialogId, InviteExchange, Leg, RequestRecord, RetainedEmission, SentRequest,
};

use super::lens::{update_dialog, update_leg};

/// The exchange of the INVITE `d` last sent.
pub fn sent_exchange(d: &Dialog) -> Option<&InviteExchange> {
    d.sent_invite.as_ref()?.invite.as_ref()
}

/// [`sent_exchange`], to update.
pub fn sent_exchange_mut(d: &mut Dialog) -> Option<&mut InviteExchange> {
    d.sent_invite.as_mut()?.invite.as_mut()
}

/// The INVITE `record` names on `leg` as it left: its own image, or the
/// leg's dialling INVITE's for that INVITE on a dialog it created. `None`
/// for a round the record never held.
pub fn invite_image<'a>(leg: &'a Leg, record: &'a RequestRecord) -> Option<&'a SentRequest> {
    record
        .sent
        .as_ref()
        .or_else(|| leg.dialled_sent().filter(|_| leg.dialled_cseq() == Some(record.outbound_cseq)))
}

/// The INVITE `d`, a dialog of `leg`, last sent, as it left.
pub fn sent_invite_image<'a>(leg: &'a Leg, d: &'a Dialog) -> Option<&'a SentRequest> {
    invite_image(leg, d.sent_invite.as_ref()?)
}

/// Whether a 2xx was taken on `d`'s last INVITE: its ACK is owed or left.
pub fn invite_taken(d: &Dialog) -> bool {
    sent_exchange(d).is_some_and(InviteExchange::taken)
}

/// Whether `d` holds an INVITE this side sent that no 2xx was taken for.
pub fn invite_untaken(d: &Dialog) -> bool {
    d.sent_invite.is_some() && !invite_taken(d)
}

/// The relay `d` last sent an INVITE for, while no final settled it — a 2xx
/// taken, or a non-2xx closing it — CANCELled toward its target or not.
pub fn invite_relay(d: &Dialog) -> Option<&RequestRecord> {
    d.sent_invite.as_ref().filter(|r| r.is_relay() && !invite_taken(d))
}

/// Whether `d`'s last INVITE is a relay still awaiting its final, not
/// CANCELled toward its target.
pub fn relayed_invite_pending(d: &Dialog) -> bool {
    invite_relay(d).is_some_and(|r| !r.cancelled)
}

/// `record`, an INVITE this side sends on `leg_id`'s dialog `id`, becomes the
/// dialog's last: the ACK the previous one's 2xx drew belongs to that
/// transaction alone (§13.2.2.4), and `record` carries it as the ACK ahead of
/// it ([`InviteExchange::ack_ahead`]) when it answered a 2xx.
pub fn open_sent_invite(
    call: Call,
    leg_id: &str,
    id: &DialogId,
    mut record: RequestRecord,
) -> Call {
    update_dialog(call, leg_id, id, |d| {
        let ahead = sent_exchange(d).filter(|x| x.ack_branch.is_some());
        let ahead = ahead.and_then(|x| x.emitted_ack.clone());
        if let Some(ahead) = ahead {
            record.invite.get_or_insert_with(InviteExchange::default).ack_ahead = Some(ahead);
        }
        d.sent_invite = Some(record);
    })
}

/// Retain the Via branch of the ACK-for-2xx just sent on `leg_id`'s
/// established dialog, so a re-ACK of a **retransmitted** 2xx reuses the SAME
/// branch (RFC 3261 §13.2.2.4): a fresh branch would mint a new client
/// transaction and never quiesce the answerer's INVITE server transaction.
/// The first write stands. A dialog holding no INVITE of its own takes a
/// record numbered `cseq`, the ACK's: a round the record never held.
pub fn retain_ack_branch(call: Call, leg_id: &str, branch: &str, cseq: i64) -> Call {
    update_leg(call, leg_id, |leg| {
        if let Some(x) = leg.dialogs.established_mut().map(|d| ensure_sent(d, cseq)) {
            x.ack_branch.get_or_insert_with(|| branch.to_string());
        }
    })
}

/// Retain the ACK-for-2xx datagram just emitted on `leg_id`'s established
/// dialog, so a re-ACK of a retransmitted 2xx re-passes THE ACK to the
/// transport (§13.2.2.4) instead of composing an equivalent: on a delayed
/// offer the retained bytes carry the answer a fresh composition would drop
/// (RFC 3264 §4). The first write stands; a dialog holding no INVITE of its
/// own takes a record numbered `cseq`, the ACK's.
pub fn retain_emitted_ack(call: Call, leg_id: &str, ack: RetainedEmission, cseq: i64) -> Call {
    update_leg(call, leg_id, |leg| {
        if let Some(x) = leg.dialogs.established_mut().map(|d| ensure_sent(d, cseq)) {
            x.emitted_ack.get_or_insert(ack);
        }
    })
}

/// The exchange of `d`'s last INVITE, recorded as a round numbered `cseq`
/// the record never held where `d` holds none.
fn ensure_sent(d: &mut Dialog, cseq: i64) -> &mut InviteExchange {
    d.sent_invite
        .get_or_insert_with(|| RequestRecord {
            origin: crate::model::RequestOrigin::Own,
            ..RequestRecord::relayed("INVITE", cseq, 0)
        })
        .invite
        .get_or_insert_with(InviteExchange::default)
}

/// Arm (`Some`) or discharge (`None`) the §13.2.2.4 relay-ACK obligation on
/// `leg_id`'s established dialog. `cseq` is the number the acknowledging
/// peer's ACK will carry — the peer's own INVITE CSeq, not this dialog's.
pub fn set_awaited_ack_cseq(call: Call, leg_id: &str, cseq: Option<i64>) -> Call {
    update_leg(call, leg_id, |leg| {
        if let Some(x) = leg.dialogs.established_mut().and_then(sent_exchange_mut) {
            x.awaited_ack_cseq = cseq;
        }
    })
}

/// Close the INVITE `leg_id`'s dialogs last sent on the client transaction
/// `branch`, which a non-2xx final answered (RFC 3261 §17.1.1.3: the
/// transaction ends with that final, hop-ACKed below the TU), so the record
/// names nothing in flight for it. Only an INVITE no 2xx was taken for is
/// closed: one taken stays for the ACK and its re-ACKs, whatever a later
/// final on the same branch says; the dialling INVITE of a leg holding no
/// confirmed dialog stays for the CANCEL.
pub fn close_rejected_invite_round(call: Call, leg_id: &str, branch: &str) -> Call {
    update_leg(call, leg_id, |leg| {
        if leg.dialogs.confirmed().is_none() {
            return;
        }
        let dialled = leg.dialled_sent().map(|s| s.branch.clone());
        let dialled_cseq = leg.dialled_cseq();
        for d in leg.dialogs.iter_mut() {
            let Some(record) = d.sent_invite.as_ref() else { continue };
            let sent_branch = match &record.sent {
                Some(sent) => Some(sent.branch.as_str()),
                None if dialled_cseq == Some(record.outbound_cseq) => dialled.as_deref(),
                None => None,
            };
            let taken = record.invite.as_ref().is_some_and(InviteExchange::taken);
            if !taken && sent_branch == Some(branch) {
                d.sent_invite = None;
            }
        }
    })
}

/// The 2xx `d` answered an INVITE with that awaits its ACK.
pub fn answered_2xx(d: &Dialog) -> Option<&crate::model::Unacked2xx> {
    d.answered_invite.as_ref()?.invite.as_ref()?.answered_2xx.as_ref()
}

/// [`answered_2xx`], to update.
pub fn answered_2xx_mut(d: &mut Dialog) -> Option<&mut crate::model::Unacked2xx> {
    d.answered_invite.as_mut()?.invite.as_mut()?.answered_2xx.as_mut()
}

/// Record on `d` the INVITE numbered `cseq` it received and answered with
/// `answer`, a 2xx awaiting its ACK.
pub fn hold_answered_2xx(d: &mut Dialog, cseq: i64, answer: crate::model::Unacked2xx) {
    let mut record = RequestRecord::received_invite(cseq);
    if let Some(x) = record.invite.as_mut() {
        x.answered_2xx = Some(answer);
    }
    d.answered_invite = Some(record);
}
