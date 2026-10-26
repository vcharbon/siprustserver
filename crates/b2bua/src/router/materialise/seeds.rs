//! The seeds a materialised call hands the transaction layer: one transaction
//! per request record the call holds, read off the `Call` alone through one
//! path, whatever its method (ADR-0055 §2). Pure — the layer takes typed seeds
//! and never learns the record (ADR-0014).
//!
//! In flight, on the record's own terms:
//! - a **client INVITE** this stack sent: a leg's dialling INVITE while it has
//!   no final, its CANCEL sent or not, and the INVITE a dialog last sent while
//!   no 2xx was taken for it — a non-2xx closes a re-INVITE's record — and,
//!   for a relay, while it was not CANCELled toward its target: such a round
//!   awaits a final only the dead node's Via can carry.
//! - a **server INVITE** this stack admitted: the a-leg's dialog-creating
//!   INVITE while it has no final, rebuilt whole so a CANCEL draws its 487
//!   under the To-tag the caller's early dialog holds, and every relayed
//!   INVITE still pending on a dialog, rebuilt from its record and keyed by
//!   the originator's top Via, so its retransmission is absorbed, a CANCEL
//!   draws 200 + 487 and a release answers it. A relay already CANCELled has
//!   had its 200 + 487 from the node that held it and is not in flight.
//! - a **server INVITE** this stack answered 2xx and still awaits the ACK for:
//!   a dialog's answered INVITE, seeded in RFC 6026 Accepted with that 2xx, so
//!   a copy of its INVITE draws the 2xx and never reaches the rules.
//! - a **client non-INVITE** for every non-INVITE request record holding a
//!   sent request, relayed or this stack's own, on a live or a retired
//!   dialog: it retransmits the request as it left under its branch, so the
//!   final its target answered the dead node with is repeated to it
//!   (RFC 3261 §17.1.2.2, §17.2.2).
//! - a **server non-INVITE** for every non-INVITE record's received half —
//!   a relay's originator's request, or a request held for a later final —
//!   rebuilt from the record, so a copy is absorbed and the final leaves on
//!   it (§17.2.2). A relay past its deadline has none: it settles as its
//!   timeout, which answers its originator nothing.

use std::collections::HashSet;

use b2bua_sdk::event::CallEvent;
use call::helpers::records_of;
use call::{Call, Dialog, Leg, RequestRecord, SentRequest, Unacked2xx};
use sip_message::generators::{generate_cancel, InviteClientTransactionHandle};
use sip_message::header::{HeaderValue, SentBy, Via};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipHeader, SipMessage, SipParser, SipRequest, SipResponse, SipStr};
use sip_txn::{ServerTxnKey, TxnSeed};

use super::received_request::rebuild_received_request;
use crate::rules::relay::rebuild_a_leg_invite;

/// Every in-flight transaction `call` names at `now_ms`, one seed per branch
/// and role. A sent non-INVITE request whose deadline passed is not in
/// flight: its record settles as a timeout ([`overdue_sent`]).
pub(super) fn seeds_for(call: &Call, now_ms: i64) -> Vec<TxnSeed> {
    let mut seeds = Vec::new();
    let mut branches = HashSet::new();
    for leg in std::iter::once(&call.a_leg).chain(call.b_legs.iter()) {
        let dialog_records = leg.dialogs.iter().flat_map(|d| {
            d.sent_invite
                .iter()
                .chain(d.answered_invite.iter())
                .chain(records_of(d).iter())
                .map(move |r| (Some(d), r))
        });
        for (dialog, record) in leg.invite.iter().map(|r| (None, r)).chain(dialog_records) {
            for seed in record_seeds(call, leg, dialog, record, now_ms) {
                // A client and a server transaction are told apart (RFC 3261
                // §17.1.3, §17.2.3): one of each may share a branch.
                let (server, via) = match &seed {
                    TxnSeed::ClientInvite { invite, .. } => (false, invite.top_via()),
                    TxnSeed::ServerInvite { request, .. } => (true, request.top_via()),
                    TxnSeed::ServerAccepted { response, .. } => (true, response.top_via()),
                    TxnSeed::ClientNonInvite { request, .. } => (false, request.top_via()),
                    TxnSeed::ServerNonInvite { request, .. } => (true, request.top_via()),
                };
                let branch = via.branch().unwrap_or_default().to_string();
                if !branch.is_empty() && branches.insert((server, branch)) {
                    seeds.push(seed);
                }
            }
        }
    }
    seeds
}

/// The seeds of one request record `leg` holds — on the leg for its
/// dialog-creating INVITE, else on `dialog`.
fn record_seeds(
    call: &Call,
    leg: &Leg,
    dialog: Option<&Dialog>,
    record: &RequestRecord,
    now_ms: i64,
) -> Vec<TxnSeed> {
    let Some(dialog) = dialog else {
        return dialog_creating_seed(call, leg, record).into_iter().collect();
    };
    if dialog.answered_invite.as_ref() == Some(record) {
        let answered = record.invite.as_ref().and_then(|x| x.answered_2xx.as_ref());
        let accepted = answered
            .and_then(parse_response)
            .map(|response| TxnSeed::ServerAccepted { response, leg_id: Some(leg.leg_id.clone()) });
        return accepted.into_iter().collect();
    }
    if record.is_invite() {
        let mut seeds: Vec<TxnSeed> = sent_invite_seed(leg, dialog, record).into_iter().collect();
        if call::helpers::relayed_invite_pending(dialog) {
            seeds.extend(relayed_server_seed(call, leg, record));
        }
        return seeds;
    }
    // A relay whose sent half is overdue settles as its timeout, which owes
    // its originator nothing (RFC 4320 §4.2): its received half is not in
    // flight either.
    let overdue_relay =
        record.is_relay() && record.sent.is_some() && remaining_ms(record, now_ms).is_none();
    let mut seeds: Vec<TxnSeed> = client_non_invite_seed(record, now_ms).into_iter().collect();
    if !record.is_own() && !overdue_relay {
        seeds.extend(server_non_invite_seed(call, leg, record));
    }
    seeds
}

/// The seed of a leg's dialog-creating INVITE while it has no final: the
/// caller's, this stack admitted, rebuilt from the call's snapshot of it; or
/// the one this stack dialled, with the CANCEL the record says was sent for
/// it while it had drawn no provisional (RFC 3261 §9.1). A record that does
/// not rebuild the caller's INVITE is never served (ADR-0052 §11): it seeds
/// none.
fn dialog_creating_seed(call: &Call, leg: &Leg, record: &RequestRecord) -> Option<TxnSeed> {
    let Some(dialled) = leg.outbound_invite() else {
        if leg.leg_id != call.a_leg.leg_id || leg.invite_final_sent().is_some() {
            return None;
        }
        let request = rebuild_a_leg_invite(&call.a_leg_invite).ok()?;
        return Some(TxnSeed::ServerInvite {
            request,
            // The tag the a-leg's own responses carry (`ensure_a_dialog`),
            // else the one the call derives for it: what a provisional an
            // earlier owner showed carried (ADR-0055 §4).
            to_tag: Some(
                leg.own_tag
                    .clone()
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| b2bua_core::caller_tags::own(call)),
            ),
            leg_id: Some(leg.leg_id.clone()),
        });
    };
    if dialled.final_received.is_some() {
        return None;
    }
    // A dialog of the leg is the record of a provisional — early, or ending
    // on a CANCEL — and a 100 leaves none. A CANCEL sent with a provisional on
    // record is on the wire already; one sent with none is still held, and
    // rides the seed (RFC 3261 §9.1).
    let provisional = !leg.dialogs.is_empty();
    let cancel = (dialled.cancel_sent && !provisional).then_some(dialled.cancel_headers.as_slice());
    client_seed(record.sent.as_ref()?, provisional, cancel)
}

/// The client seed of the INVITE `leg`'s `dialog` last sent, `record`: while
/// the leg's dialling INVITE has no final, or no 2xx was taken for it and,
/// for a relay, it was not CANCELled toward its target. A re-INVITE records
/// no provisional.
fn sent_invite_seed(leg: &Leg, dialog: &Dialog, record: &RequestRecord) -> Option<TxnSeed> {
    let sent = call::helpers::invite_image(leg, record)?;
    let initial_in_flight = leg.outbound_invite().is_some_and(|i| i.final_received.is_none());
    let round_open = !call::helpers::invite_taken(dialog) && !record.cancelled;
    if !(initial_in_flight || round_open) {
        return None;
    }
    let provisional = initial_in_flight && !leg.dialogs.is_empty();
    client_seed(sent, provisional, None)
}

/// Whether the record is an INVITE's, whose transactions the INVITE seeds
/// above rebuild.
fn invite_record(record: &RequestRecord) -> bool {
    record.method == "INVITE"
}

/// The non-INVITE record of `call` whose received half `branch` names: a
/// request a relay was made for, or one held for a later final. A copy of it
/// is no new request.
pub(in crate::router) fn received_record<'a>(
    call: &'a Call,
    branch: &str,
) -> Option<&'a RequestRecord> {
    if branch.is_empty() {
        return None;
    }
    call::helpers::every_record(call)
        .filter(|(_, _, r)| !r.is_own() && !invite_record(r))
        .find(|(_, _, r)| pending_via(r).is_some_and(|v| v.branch() == Some(branch)))
        .map(|(_, _, r)| r)
}

/// `call` without the relay a copy of its originator's request on `branch`
/// supersedes: the relay's final returns to another node, so the copy is
/// relayed afresh and that relay settles nothing here.
pub(in crate::router) fn drop_superseded(mut call: Call, branch: &str) -> Call {
    let superseded = |r: &RequestRecord| {
        r.is_relay() && pending_via(r).is_some_and(|v| v.branch() == Some(branch))
    };
    for leg in std::iter::once(&mut call.a_leg).chain(call.b_legs.iter_mut()) {
        for d in leg.dialogs.iter_mut() {
            d.requests.retain(|r| !superseded(r));
            if let Some(retired) = d.retired_mut() {
                retired.open_requests.retain(|r| !superseded(r));
            }
        }
    }
    call
}

/// The client seed of a sent non-INVITE request: the request as it left and
/// the wire destination it was sent to. `None` when neither reads back.
fn client_non_invite_seed(record: &RequestRecord, now_ms: i64) -> Option<TxnSeed> {
    let sent = record.sent.as_ref()?;
    let bound_ms = remaining_ms(record, now_ms)?;
    let request = parse_request(&sent.image)?;
    let dest = sip_net::ip_socket_addr(&sent.destination.host, sent.destination.port)?;
    Some(TxnSeed::ClientNonInvite { request, dest, bound_ms })
}

/// What is left at `now_ms` of the Timer F `record`'s sent request started;
/// `None` once its deadline passed.
fn remaining_ms(record: &RequestRecord, now_ms: i64) -> Option<u64> {
    let full = RequestRecord::TIMER_F_MS;
    let left = record.deadline_ms.map_or(full, |at| at.saturating_sub(now_ms)).min(full);
    u64::try_from(left).ok().filter(|left| *left > 0)
}

/// The timeout each sent non-INVITE request of `call` whose deadline passed
/// by `now_ms` owes the call: its transaction gave up on the node that sent
/// it, or would have (RFC 3261 §17.1.2.2), so the copy settles its record as
/// that node's timeout would have.
pub(super) fn overdue_sent(call: &Call, now_ms: i64) -> Vec<CallEvent> {
    call::helpers::every_record(call)
        .filter(|(_, _, r)| !invite_record(r) && r.sent.is_some())
        .filter(|(_, _, r)| remaining_ms(r, now_ms).is_none())
        .filter_map(|(leg, _, r)| {
            let sent = r.sent.as_ref()?;
            Some(CallEvent::Timeout {
                branch: sent.branch.clone(),
                call_ref: Some(call.call_ref.clone()),
                leg_id: Some(leg.leg_id.clone()),
                method: Some(r.method.clone()),
                cseq: u32::try_from(r.outbound_cseq).ok()?,
                destination: None,
                timeout_kind: call::TxnTimeoutKind::Response,
                incarnation: Some(call.incarnation().to_string()),
            })
        })
        .collect()
}

/// The server seed of a non-INVITE record's received half: the request
/// rebuilt from the record, attributed to the leg it arrived on — a relay's
/// originator is the peer of the leg it was relayed toward.
fn server_non_invite_seed(call: &Call, leg: &Leg, record: &RequestRecord) -> Option<TxnSeed> {
    let request = rebuild_received_request(record)?;
    let arrived_on = if record.is_held() {
        leg.leg_id.clone()
    } else {
        call::helpers::get_peer(call, &leg.leg_id)
            .map(str::to_string)
            .unwrap_or_else(|| call.a_leg.leg_id.clone())
    };
    Some(TxnSeed::ServerNonInvite { request, leg_id: Some(arrived_on) })
}

/// Whether `branch` names a server seed an INVITE can be a copy of: a relayed
/// INVITE still pending on one of `call`'s dialogs, or an INVITE a dialog
/// answered 2xx, the answered initial INVITE's included. The a-leg's
/// unanswered initial INVITE is not named: on the node serving the call the
/// layer's own transaction absorbs its retransmission below the router.
pub(in crate::router) fn names_server_branch(call: &Call, branch: &str) -> bool {
    if branch.is_empty() {
        return false;
    }
    let dialogs =
        || std::iter::once(&call.a_leg).chain(&call.b_legs).flat_map(|l| l.dialogs.iter());
    let relayed = dialogs()
        .filter(|d| call::helpers::relayed_invite_pending(d))
        .filter_map(|d| d.sent_invite.as_ref())
        .filter_map(pending_via)
        .any(|v| v.branch() == Some(branch));
    relayed
        || dialogs()
            .filter_map(call::helpers::answered_2xx)
            .filter_map(parse_response)
            .any(|r| r.top_via().branch() == Some(branch))
}

/// The INVITE server transaction the originator's request of the pending
/// relayed re-INVITE `p` opened here: its top-Via branch and sent-by.
pub(in crate::router) fn relayed_server_key(p: &RequestRecord) -> Option<ServerTxnKey> {
    let via = pending_via(p)?;
    let branch = via.branch().filter(|b| !b.is_empty())?.to_string();
    Some(ServerTxnKey::invite(branch, SentBy::of(&via)))
}

/// The originator's top Via a pending relayed request was received with.
fn pending_via(p: &RequestRecord) -> Option<Via> {
    let top_via = p.source_vias.first()?;
    Via::parse(&SipStr::owned(top_via)).ok()
}

/// The client seed of the INVITE `sent`: the INVITE as it left, the wire
/// destination it was sent to, whether the record holds a provisional for it,
/// and the CANCEL of it the record says was sent, rebuilt stating `cancel`'s
/// headers (RFC 3261 §9.1). `None` where the image or destination does not
/// read, or the CANCEL does not rebuild — nothing can be rebuilt from it.
fn client_seed(
    sent: &SentRequest,
    provisional: bool,
    cancel: Option<&[(String, String)]>,
) -> Option<TxnSeed> {
    let invite = parse_request(&sent.image)?;
    let dest = sip_net::ip_socket_addr(&sent.destination.host, sent.destination.port)?;
    let cancel = cancel.map(|stated| {
        let stated: Vec<SipHeader> = stated
            .iter()
            .map(|(name, value)| SipHeader {
                name: name.clone().into(),
                value: value.clone().into(),
            })
            .collect();
        generate_cancel(&InviteClientTransactionHandle { original_invite: invite.clone() }, &stated)
            .map(Box::new)
    });
    let cancel = cancel
        .transpose()
        .inspect_err(|e| {
            tracing::warn!(call_id = %invite.call_id(), error = %e,
                "client transaction not seeded: its CANCEL makes no message");
        })
        .ok()?;
    Some(TxnSeed::ClientInvite { invite, dest, provisional, cancel })
}

/// The server seed for a relayed INVITE pending on one of `leg`'s dialogs:
/// the originator's request rebuilt from the snapshot, keyed by its top-Via
/// branch and sent-by, answered under the To-tag its To already carries,
/// attributed to the originator's leg.
fn relayed_server_seed(call: &Call, leg: &Leg, pending: &RequestRecord) -> Option<TxnSeed> {
    let request = rebuild_received_request(pending)?;
    let to_tag = request.to().tag().map(str::to_string);
    let originator = call::helpers::get_peer(call, &leg.leg_id)
        .map(str::to_string)
        .unwrap_or_else(|| call.a_leg.leg_id.clone());
    Some(TxnSeed::ServerInvite { request, to_tag, leg_id: Some(originator) })
}

/// A request re-parsed from the bytes a record keeps it as.
fn parse_request(bytes: &[u8]) -> Option<SipRequest> {
    match CustomParser::new().parse(bytes).ok()? {
        SipMessage::Request(r) => Some(r),
        SipMessage::Response(_) => None,
    }
}

/// The retained 2xx re-parsed from the datagram it left as.
fn parse_response(retained: &Unacked2xx) -> Option<SipResponse> {
    match CustomParser::new().parse(retained.emission.wire().0).ok()? {
        SipMessage::Response(r) => Some(r),
        SipMessage::Request(_) => None,
    }
}

#[cfg(test)]
mod tests;
