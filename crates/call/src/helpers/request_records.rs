//! The request records a call's dialogs hold (ADR-0055 §2): adding one,
//! finding the one a response or a transaction timeout names, closing it, and
//! the records a retired dialog still holds.

use crate::model::{Call, Dialog, DialogId, Leg, RequestRecord};

use super::leg::find_leg;
use super::lens::update_dialog;

/// Add `record` to `leg_id`'s dialog `id`.
pub fn add_record(call: Call, leg_id: &str, id: &DialogId, record: RequestRecord) -> Call {
    update_dialog(call, leg_id, id, |d| d.requests.push(record))
}

/// Where a request record is kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordHolder {
    /// Among the requests of the leg's dialog with this identity.
    Dialog(DialogId),
    /// Among the open requests of the leg's retired dialog with this identity.
    Retired(DialogId),
}

/// The records `d` holds open, live or held by its retirement.
pub fn records_of(d: &Dialog) -> &[RequestRecord] {
    d.retired().map_or(d.requests.as_slice(), |r| r.open_requests.as_slice())
}

fn holder_of(d: &Dialog) -> RecordHolder {
    match d.retired() {
        Some(_) => RecordHolder::Retired(d.id()),
        None => RecordHolder::Dialog(d.id()),
    }
}

/// The sent record a final for `method` numbered `cseq` arriving on `leg_id`
/// answers (RFC 3261 §8.1.3.3), and where it is kept: on the dialog the
/// response's tags name, or among that dialog's open requests once it
/// retired. A response with no To-tag reads the leg's confirmed dialog. Never
/// another dialog's record: two dialogs may carry the same CSeq.
pub fn answered_record(
    call: &Call,
    leg_id: &str,
    to: Option<&str>,
    from: Option<&str>,
    cseq: i64,
    method: &str,
) -> Option<(RecordHolder, RequestRecord)> {
    let leg = find_leg(call, leg_id)?;
    let d = match to {
        None => leg.dialogs.confirmed()?,
        Some(_) => leg.dialogs.of_response(to, from)?,
    };
    records_of(d).iter().find(|r| r.answered_by(method, cseq)).map(|r| (holder_of(d), r.clone()))
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
        records_of(d).iter().find(|r| r.branch() == Some(branch)).map(|r| (holder_of(d), r.clone()))
    })
}

/// Close the sent record `holder` keeps with outbound CSeq `cseq` on
/// `leg_id`: its transaction settled.
pub fn close_record(call: Call, leg_id: &str, holder: &RecordHolder, cseq: i64) -> Call {
    let sent = |r: &RequestRecord| r.is_sent() && r.outbound_cseq == cseq;
    match holder {
        RecordHolder::Dialog(id) => {
            update_dialog(call, leg_id, id, |d| d.requests.retain(|r| !sent(r)))
        }
        RecordHolder::Retired(id) => update_dialog(call, leg_id, id, |d| {
            if let Some(r) = d.retired_mut() {
                r.open_requests.retain(|r| !sent(r));
            }
        }),
    }
}

/// Close the held record `holder` keeps for the received request numbered
/// `cseq` on `leg_id`: its final left.
pub fn close_held(call: Call, leg_id: &str, holder: &RecordHolder, cseq: i64) -> Call {
    let held = |r: &RequestRecord| r.is_held() && r.inbound_cseq == cseq;
    match holder {
        RecordHolder::Dialog(id) => {
            update_dialog(call, leg_id, id, |d| d.requests.retain(|r| !held(r)))
        }
        RecordHolder::Retired(id) => update_dialog(call, leg_id, id, |d| {
            if let Some(r) = d.retired_mut() {
                r.open_requests.retain(|r| !held(r));
            }
        }),
    }
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
        records_of(d)
            .iter()
            .find(|r| {
                r.is_held() && r.inbound_cseq == cseq && r.method.eq_ignore_ascii_case(method)
            })
            .map(|r| (holder_of(d), r.clone()))
    })
}

/// Take every relay and held record still open in `leg`'s retired dialogs,
/// each with its dialog: the leg ends, so no final reaches them but the one
/// this stack answers (RFC 3261 §8.2.6). Own records stay until they settle.
pub fn take_retired_owed(leg: &mut Leg) -> Vec<(DialogId, RequestRecord)> {
    let mut taken = Vec::new();
    for d in leg.dialogs.iter_mut() {
        let id = d.id();
        if let Some(r) = d.retired_mut() {
            let (owed, own): (Vec<_>, Vec<_>) =
                std::mem::take(&mut r.open_requests).into_iter().partition(|r| !r.is_own());
            r.open_requests = own;
            taken.extend(owed.into_iter().map(|p| (id.clone(), p)));
        }
    }
    taken
}

/// The relay `dialog` holds with outbound CSeq `outbound_cseq`.
pub fn find_relay(dialog: &Dialog, outbound_cseq: i64) -> Option<&RequestRecord> {
    dialog.requests.iter().find(|p| p.is_relay() && p.outbound_cseq == outbound_cseq)
}

/// Mark a relay CANCELled (RFC 3261 §9): the relayed (re-)INVITE it records
/// has been CANCELled toward its target, so the eventual final response
/// resolves locally instead of being relayed back.
pub fn cancel_relay(call: Call, leg_id: &str, id: &DialogId, outbound_cseq: i64) -> Call {
    update_dialog(call, leg_id, id, |d| {
        if let Some(p) =
            d.requests.iter_mut().find(|p| p.is_relay() && p.outbound_cseq == outbound_cseq)
        {
            p.cancelled = true;
        }
    })
}

/// Remove the relay `leg_id`'s dialog `id` holds with outbound CSeq
/// `outbound_cseq`.
pub fn remove_relay(call: Call, leg_id: &str, id: &DialogId, outbound_cseq: i64) -> Call {
    update_dialog(call, leg_id, id, |d| {
        d.requests.retain(|p| !(p.is_relay() && p.outbound_cseq == outbound_cseq))
    })
}

/// Every record of `call`, with its leg: live dialogs' and retired ones'.
pub fn every_record(call: &Call) -> impl Iterator<Item = (&Leg, &Dialog, &RequestRecord)> {
    std::iter::once(&call.a_leg)
        .chain(call.b_legs.iter())
        .flat_map(|leg| leg.dialogs.iter().map(move |d| (leg, d)))
        .flat_map(|(leg, d)| records_of(d).iter().map(move |r| (leg, d, r)))
}

/// `call` with every held record whose deadline is at or before `now_ms`
/// taken out, each with its leg and dialog: the platform answers them.
pub fn take_overdue_held(
    mut call: Call,
    now_ms: i64,
) -> (Call, Vec<(String, RecordHolder, RequestRecord)>) {
    let overdue: Vec<(String, RecordHolder, RequestRecord)> = every_record(&call)
        .filter(|(_, _, r)| r.is_held() && r.deadline_ms.is_some_and(|at| at <= now_ms))
        .map(|(leg, d, r)| (leg.leg_id.clone(), holder_of(d), r.clone()))
        .collect();
    for (leg_id, holder, r) in &overdue {
        call = close_held(call, leg_id, holder, r.inbound_cseq);
    }
    (call, overdue)
}

/// The earliest deadline of a held record of `call`.
pub fn next_held_deadline(call: &Call) -> Option<i64> {
    every_record(call).filter(|(_, _, r)| r.is_held()).filter_map(|(_, _, r)| r.deadline_ms).min()
}
