//! The request records a call's dialogs hold (ADR-0055 §2): adding one,
//! finding the one a response or a transaction timeout names, and closing it,
//! on a live dialog or a retired one alike. A dialog's INVITE exchanges are
//! records of their own ([`super::invite_records`]); a final finds a relayed
//! INVITE here as it finds any relay.

use crate::model::{Call, Dialog, DialogId, Leg, RequestRecord};

use super::leg::find_leg;
use super::lens::update_dialog;

/// Add `record` to `leg_id`'s dialog `id`.
pub fn add_record(call: &mut Call, leg_id: &str, id: &DialogId, record: RequestRecord) {
    update_dialog(call, leg_id, id, |d| d.requests.push(record))
}

/// The dialog a request record is kept on, and whether it retired.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordHolder {
    /// The leg's live or ended dialog with this identity.
    Dialog(DialogId),
    /// The leg's retired dialog with this identity.
    Retired(DialogId),
}

impl RecordHolder {
    /// The identity of the dialog holding the record.
    pub fn id(&self) -> &DialogId {
        match self {
            RecordHolder::Dialog(id) | RecordHolder::Retired(id) => id,
        }
    }
}

fn holder_of(d: &Dialog) -> RecordHolder {
    if d.is_retired() {
        RecordHolder::Retired(d.id())
    } else {
        RecordHolder::Dialog(d.id())
    }
}

/// What a response answers: the method and number of its CSeq, and the
/// top-Via branch of the client transaction it answers.
#[derive(Clone, Copy, Debug)]
pub struct Answers<'a> {
    pub method: &'a str,
    pub cseq: i64,
    pub branch: Option<&'a str>,
}

/// The sent record a final arriving on `leg_id` answers (RFC 3261
/// §8.1.3.3), and where it is kept: on the dialog the response's tags name,
/// retired or not. A response with no
/// To-tag reads the leg's confirmed dialog. Never another dialog's record: two
/// dialogs may carry the same CSeq.
pub fn answered_record(
    call: &Call,
    leg_id: &str,
    to: Option<&str>,
    from: Option<&str>,
    answers: Answers<'_>,
) -> Option<(RecordHolder, RequestRecord)> {
    let leg = find_leg(call, leg_id)?;
    let d = match to {
        None => leg.dialogs.confirmed()?,
        Some(_) => leg.dialogs.of_response(to, from)?,
    };
    d.requests
        .iter()
        .chain(d.sent_invite.iter())
        .find(|r| r.answered_by(answers.method, answers.cseq, answers.branch))
        .map(|r| (holder_of(d), r.clone()))
}

/// The sent record whose transaction left on `branch` from `leg_id`, and
/// where it is kept: what a transaction timeout names (RFC 3261 §17.1.2.2).
pub fn record_with_branch(
    call: &Call,
    leg_id: &str,
    branch: &str,
) -> Option<(RecordHolder, RequestRecord)> {
    if branch.is_empty() {
        return None;
    }
    let leg = find_leg(call, leg_id)?;
    leg.dialogs.iter().find_map(|d| {
        d.requests.iter().find(|r| r.branch() == Some(branch)).map(|r| (holder_of(d), r.clone()))
    })
}

/// Close the sent non-INVITE record `holder` keeps with outbound CSeq `cseq`
/// on `leg_id`: its transaction settled. An INVITE's record stays for the
/// ACK its 2xx owes; a non-2xx closes it
/// ([`super::close_rejected_invite_round`]).
pub fn close_record(call: &mut Call, leg_id: &str, holder: &RecordHolder, cseq: i64) {
    let sent = |r: &RequestRecord| r.is_sent() && r.outbound_cseq == cseq;
    update_dialog(call, leg_id, holder.id(), |d| d.requests.retain(|r| !sent(r)))
}

/// Close the held record `holder` keeps for the received request numbered
/// `cseq` on `leg_id`: its final left.
pub fn close_held(call: &mut Call, leg_id: &str, holder: &RecordHolder, cseq: i64) {
    let held = |r: &RequestRecord| r.is_held() && r.inbound_cseq == cseq;
    update_dialog(call, leg_id, holder.id(), |d| d.requests.retain(|r| !held(r)))
}

/// The held record on `leg_id` for the received request `method` numbered
/// `cseq`, and where it is kept.
pub fn held_record(
    call: &Call,
    leg_id: &str,
    method: &str,
    cseq: i64,
) -> Option<(RecordHolder, RequestRecord)> {
    let leg = find_leg(call, leg_id)?;
    leg.dialogs.iter().find_map(|d| {
        d.requests
            .iter()
            .find(|r| r.is_held() && r.inbound_cseq == cseq && r.method == method)
            .map(|r| (holder_of(d), r.clone()))
    })
}

/// The relay `dialog` holds with outbound CSeq `outbound_cseq` awaiting its
/// final: a non-INVITE's, or the INVITE it last sent.
pub fn find_relay(dialog: &Dialog, outbound_cseq: i64) -> Option<&RequestRecord> {
    dialog
        .requests
        .iter()
        .chain(super::invite_records::invite_relay(dialog))
        .find(|p| p.is_relay() && p.outbound_cseq == outbound_cseq)
}

/// Mark a relayed INVITE CANCELled (RFC 3261 §9): it has been CANCELled
/// toward its target, so the eventual final response resolves locally instead
/// of being relayed back.
pub fn cancel_relay(call: &mut Call, leg_id: &str, id: &DialogId, outbound_cseq: i64) {
    update_dialog(call, leg_id, id, |d| {
        let relay = d.sent_invite.as_mut().filter(|p| p.is_relay());
        if let Some(p) = relay.filter(|p| p.outbound_cseq == outbound_cseq) {
            p.cancelled = true;
        }
    })
}

/// Close the relay `leg_id`'s dialog `id` holds with outbound CSeq
/// `outbound_cseq`: a relayed INVITE no 2xx was taken for is closed, one taken
/// stays for the ACK and its re-ACKs.
pub fn remove_relay(call: &mut Call, leg_id: &str, id: &DialogId, outbound_cseq: i64) {
    update_dialog(call, leg_id, id, |d| {
        d.requests.retain(|p| !(p.is_relay() && p.outbound_cseq == outbound_cseq));
        if super::invite_records::invite_relay(d).is_some_and(|p| p.outbound_cseq == outbound_cseq)
        {
            d.sent_invite = None;
        }
    })
}

/// Every record of `call`, with its leg and dialog.
pub fn every_record(call: &Call) -> impl Iterator<Item = (&Leg, &Dialog, &RequestRecord)> {
    std::iter::once(&call.a_leg)
        .chain(call.b_legs.iter())
        .flat_map(|leg| leg.dialogs.iter().map(move |d| (leg, d)))
        .flat_map(|(leg, d)| d.requests.iter().map(move |r| (leg, d, r)))
}

/// Take every held record whose deadline is at or before `now_ms` out of
/// `call` and return them, each with its leg and dialog: the platform
/// answers them.
pub fn take_overdue_held(
    call: &mut Call,
    now_ms: i64,
) -> Vec<(String, RecordHolder, RequestRecord)> {
    let overdue: Vec<(String, RecordHolder, RequestRecord)> = every_record(call)
        .filter(|(_, _, r)| r.is_held() && r.deadline_ms.is_some_and(|at| at <= now_ms))
        .map(|(leg, d, r)| (leg.leg_id.clone(), holder_of(d), r.clone()))
        .collect();
    for (leg_id, holder, r) in &overdue {
        close_held(call, leg_id, holder, r.inbound_cseq);
    }
    overdue
}

/// The earliest deadline of a held record of `call`.
pub fn next_held_deadline(call: &Call) -> Option<i64> {
    every_record(call).filter(|(_, _, r)| r.is_held()).filter_map(|(_, _, r)| r.deadline_ms).min()
}
