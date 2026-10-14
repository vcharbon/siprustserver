//! The seeds a materialised call hands the transaction layer: the in-flight
//! INVITE transactions the record names, read off the `Call` alone. Pure — the
//! layer takes typed seeds and never learns the record (ADR-0014).
//!
//! In flight, on the record's own terms:
//! - a **client INVITE** this stack sent: the initial INVITE of a leg still
//!   `Trying` / `Early` (its handle on the leg and on every early dialog), and
//!   a re-INVITE whose round is still open on its confirmed dialog, whichever
//!   side originated it. A handle names an open round while no 2xx has been
//!   taken for it (`ack_branch` / `awaited_ack_cseq` unset — the handle stays
//!   after a 2xx for the ACK and its re-ACKs) and the record has not closed it
//!   (a non-2xx final drops it, `close_rejected_invite_round`); a relayed round
//!   already CANCELled toward its target awaits a final only the dead node's
//!   Via can carry, so it is not in flight here.
//! - a **server INVITE** this stack admitted: the a-leg's initial INVITE while
//!   the a-leg is unanswered, rebuilt whole so a CANCEL draws its 487 under the
//!   To-tag the caller's early dialog holds, and every relayed INVITE still
//!   pending on a dialog, rebuilt from its snapshot and keyed by the
//!   originator's top Via, so its retransmission is absorbed, a CANCEL draws
//!   200 + 487 and a release answers it. A pending request already CANCELled
//!   has had its 200 + 487 from the node that held it and is not in flight.
//! - a **server INVITE** this stack answered 2xx and still awaits the ACK for:
//!   a retained 2xx on a dialog (`answered_2xx`, `pending_reinvite_2xx`),
//!   seeded in RFC 6026 Accepted with that 2xx, so a copy of its INVITE draws
//!   the 2xx and never reaches the rules.

use std::collections::HashSet;
use std::net::SocketAddr;

use call::{Call, Dialog, InviteTxnHandle, Leg, PendingRequest, Unacked2xx};
use sip_message::generators::{generate_cancel, InviteClientTransactionHandle};
use sip_message::header::{HeaderValue, SentBy, Via};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipHeader, SipMessage, SipParser, SipRequest, SipResponse, SipStr};
use sip_txn::{ServerTxnKey, TxnSeed};

use super::relayed_invite::rebuild_relayed_invite;
use crate::rules::relay::rebuild_a_leg_invite;

/// Every in-flight INVITE transaction `call` names, one seed per branch.
pub(super) fn seeds_for(call: &Call) -> Vec<TxnSeed> {
    let mut seeds = Vec::new();
    let mut branches = HashSet::new();
    let mut push = |seed: TxnSeed| {
        // A client and a server transaction are told apart (RFC 3261 §17.1.3,
        // §17.2.3): one of each may share a branch.
        let (server, via) = match &seed {
            TxnSeed::ClientInvite { invite, .. } => (false, invite.top_via()),
            TxnSeed::ServerInvite { request, .. } => (true, request.top_via()),
            TxnSeed::ServerAccepted { response, .. } => (true, response.top_via()),
        };
        let branch = via.branch().unwrap_or_default().to_string();
        if !branch.is_empty() && branches.insert((server, branch)) {
            seeds.push(seed);
        }
    };

    if call.a_leg.invite_final_sent.is_none() {
        push(TxnSeed::ServerInvite {
            request: rebuild_a_leg_invite(&call.a_leg_invite),
            // The tag the a-leg's own responses carry (`ensure_a_dialog`);
            // none while nothing above a 100 went out.
            to_tag: call.a_leg.own_tag.clone().filter(|t| !t.is_empty()),
            leg_id: Some(call.a_leg.leg_id.clone()),
        });
    }

    for leg in std::iter::once(&call.a_leg).chain(call.b_legs.iter()) {
        // The INVITE that dialled the leg is in flight until its final,
        // its CANCEL sent or not (RFC 3261 §9.1).
        let initial = leg.outbound_invite.as_ref().filter(|i| i.final_received.is_none());
        let initial_in_flight = initial.is_some();
        // A dialog of the leg is the record of a provisional — early, or
        // ending on a CANCEL — and a 100 leaves none. A CANCEL sent with a
        // provisional on record is on the wire already; one sent with none is
        // still held, and rides the seed (RFC 3261 §9.1).
        let provisional = !leg.dialogs.is_empty();
        if let Some(i) = initial {
            let cancel = (i.cancel_sent && !provisional).then_some(i.cancel_headers.as_slice());
            if let Some(seed) = i.txn.as_ref().and_then(|t| client_seed(t, provisional, cancel)) {
                push(seed);
            }
        }
        for dialog in &leg.dialogs {
            if let Some(handle) = &dialog.pending_invite_txn {
                if initial_in_flight || round_open(dialog, handle) {
                    // A re-INVITE round records no provisional.
                    if let Some(seed) = client_seed(handle, initial_in_flight && provisional, None)
                    {
                        push(seed);
                    }
                }
            }
            for pending in dialog.inbound_pending_requests.iter().filter(|p| pending_invite(p)) {
                if let Some(seed) = relayed_server_seed(call, leg, pending) {
                    push(seed);
                }
            }
            for retained in retained_2xx(dialog) {
                if let Some(response) = parse_response(retained) {
                    push(TxnSeed::ServerAccepted { response, leg_id: Some(leg.leg_id.clone()) });
                }
            }
        }
    }
    seeds
}

/// Whether `branch` names a server seed an in-dialog INVITE can be a copy of:
/// a relayed INVITE still pending on one of `call`'s dialogs, or an INVITE
/// whose 2xx a dialog retains. The a-leg's unanswered initial INVITE is not
/// asked about: its retransmission carries no To-tag, resolves as an initial
/// INVITE and never reaches the in-dialog re-offer; the layer's own seed
/// absorbs it below the router.
pub(super) fn names_server_branch(call: &Call, branch: &str) -> bool {
    if branch.is_empty() {
        return false;
    }
    let dialogs =
        || std::iter::once(&call.a_leg).chain(&call.b_legs).flat_map(|l| l.dialogs.iter());
    let relayed = dialogs()
        .flat_map(|d| d.inbound_pending_requests.iter())
        .filter(|p| pending_invite(p))
        .filter_map(pending_via)
        .any(|v| v.branch() == Some(branch));
    relayed
        || dialogs()
            .flat_map(retained_2xx)
            .filter_map(parse_response)
            .any(|r| r.top_via().branch() == Some(branch))
}

/// The 2xx finals `dialog` retains while their ACK is awaited (RFC 3261
/// §13.3.1.4): the call's answer and a relayed re-INVITE's.
fn retained_2xx(dialog: &Dialog) -> impl Iterator<Item = &Unacked2xx> {
    dialog.answered_2xx.iter().chain(dialog.pending_reinvite_2xx.iter())
}

/// A re-INVITE round on a confirmed dialog still open for `handle`'s INVITE:
/// no 2xx taken for it, and — for a relayed round — its snapshot not already
/// CANCELled toward the target.
fn round_open(dialog: &Dialog, handle: &InviteTxnHandle) -> bool {
    if dialog.ack_branch.is_some() || dialog.awaited_ack_cseq.is_some() {
        return false;
    }
    let Some(cseq) = parse_request(&handle.original_invite).map(|r| r.cseq().seq() as i64) else {
        return false;
    };
    dialog
        .inbound_pending_requests
        .iter()
        .filter(|p| p.method.eq_ignore_ascii_case("INVITE") && p.outbound_cseq == cseq)
        .all(|p| !p.cancelled)
}

/// The INVITE server transaction the originator's request of the pending
/// relayed re-INVITE `p` opened here: its top-Via branch and sent-by.
pub(in crate::router) fn relayed_server_key(p: &PendingRequest) -> Option<ServerTxnKey> {
    let via = pending_via(p)?;
    let branch = via.branch().filter(|b| !b.is_empty())?.to_string();
    Some(ServerTxnKey::invite(branch, SentBy::of(&via)))
}

/// The originator's top Via a pending relayed request was received with.
fn pending_via(p: &PendingRequest) -> Option<Via> {
    let top_via = p.source_vias.first()?;
    Via::parse(&SipStr::owned(top_via)).ok()
}

fn pending_invite(p: &PendingRequest) -> bool {
    p.method.eq_ignore_ascii_case("INVITE") && !p.cancelled
}

/// The client seed for an INVITE handle: the retained INVITE, the wire
/// destination it was sent to, whether the record holds a provisional for it,
/// and the CANCEL of it the record says was sent, rebuilt stating
/// `cancel`'s headers (RFC 3261 §9.1). `None` for a handle whose bytes or
/// destination do not read — nothing can be rebuilt from it.
fn client_seed(
    handle: &InviteTxnHandle,
    provisional: bool,
    cancel: Option<&[(String, String)]>,
) -> Option<TxnSeed> {
    let invite = parse_request(&handle.original_invite)?;
    let dest: SocketAddr =
        format!("{}:{}", handle.destination.host, handle.destination.port).parse().ok()?;
    let cancel = cancel.map(|stated| {
        let stated: Vec<SipHeader> = stated
            .iter()
            .map(|(name, value)| SipHeader {
                name: name.clone().into(),
                value: value.clone().into(),
            })
            .collect();
        Box::new(generate_cancel(
            &InviteClientTransactionHandle { original_invite: invite.clone() },
            &stated,
        ))
    });
    Some(TxnSeed::ClientInvite { invite, dest, provisional, cancel })
}

/// The server seed for a relayed INVITE pending on one of `leg`'s dialogs:
/// the originator's request rebuilt from the snapshot, keyed by its top-Via
/// branch and sent-by, answered under the To-tag its To already carries,
/// attributed to the originator's leg.
fn relayed_server_seed(call: &Call, leg: &Leg, pending: &PendingRequest) -> Option<TxnSeed> {
    let request = rebuild_relayed_invite(pending)?;
    let to_tag = request.to().tag().map(str::to_string);
    let originator = call::helpers::get_peer(call, &leg.leg_id)
        .map(str::to_string)
        .unwrap_or_else(|| call.a_leg.leg_id.clone());
    Some(TxnSeed::ServerInvite { request, to_tag, leg_id: Some(originator) })
}

/// A request re-parsed from the bytes an INVITE handle caches it as.
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
