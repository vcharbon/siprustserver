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
//!   pending on a dialog, keyed by the originator's top Via with no request
//!   (only the retransmission is absorbed). A pending request already
//!   CANCELled has had its 200 + 487 from the node that held it and is not in
//!   flight.

use std::collections::HashSet;
use std::net::SocketAddr;

use call::{Call, Dialog, InviteTxnHandle, Leg, PendingRequest};
use sip_message::header::{self, HeaderValue, SentBy, Via};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest, SipStr};
use sip_txn::TxnSeed;

use crate::rules::relay::rebuild_a_leg_invite;

/// Every in-flight INVITE transaction `call` names, one seed per branch.
pub(super) fn seeds_for(call: &Call) -> Vec<TxnSeed> {
    let mut seeds = Vec::new();
    let mut branches = HashSet::new();
    let mut push = |seed: TxnSeed| {
        // A client and a server transaction are told apart (RFC 3261 §17.1.3,
        // §17.2.3): one of each may share a branch.
        let (server, branch) = match &seed {
            TxnSeed::ClientInvite { invite, .. } => {
                (false, invite.top_via().branch().unwrap_or_default().to_string())
            }
            TxnSeed::ServerInvite { branch, .. } => (true, branch.clone()),
        };
        if !branch.is_empty() && branches.insert((server, branch)) {
            seeds.push(seed);
        }
    };

    if call.a_leg.invite_final_sent.is_none() {
        let invite = rebuild_a_leg_invite(&call.a_leg_invite);
        push(TxnSeed::ServerInvite {
            branch: invite.top_via().branch().unwrap_or_default().to_string(),
            sent_by: SentBy::of(invite.top_via()),
            call_id: invite.call_id().as_str().to_string(),
            from_tag: invite.from().tag().unwrap_or_default().to_string(),
            // The tag the a-leg's own responses carry (`ensure_a_dialog`);
            // none while nothing above a 100 went out.
            to_tag: call.a_leg.own_tag.clone().filter(|t| !t.is_empty()),
            leg_id: Some(call.a_leg.leg_id.clone()),
            original_request: Some(invite),
        });
    }

    for leg in std::iter::once(&call.a_leg).chain(call.b_legs.iter()) {
        // The INVITE that dialled the leg is in flight until its final,
        // its CANCEL sent or not (RFC 3261 §9.1).
        let initial = leg.outbound_invite.as_ref().filter(|i| i.final_received.is_none());
        let initial_in_flight = initial.is_some();
        // An early dialog is the record of a provisional; a 100 leaves none.
        let provisional = leg.state() == call::LegState::Early;
        if let Some(seed) =
            initial.and_then(|i| i.txn.as_ref()).and_then(|t| client_seed(t, provisional))
        {
            push(seed);
        }
        for dialog in &leg.dialogs {
            if let Some(handle) = &dialog.pending_invite_txn {
                if initial_in_flight || round_open(dialog, handle) {
                    // A re-INVITE round records no provisional.
                    if let Some(seed) = client_seed(handle, initial_in_flight && provisional) {
                        push(seed);
                    }
                }
            }
            for pending in dialog.inbound_pending_requests.iter().filter(|p| pending_invite(p)) {
                if let Some(seed) = relayed_server_seed(call, leg, pending) {
                    push(seed);
                }
            }
        }
    }
    seeds
}

/// Whether `branch` is a relayed INVITE still pending on one of `call`'s
/// dialogs — the one server seed an in-dialog INVITE can name. The a-leg's
/// initial INVITE is not asked about: its retransmission carries no To-tag,
/// resolves as an initial INVITE and never reaches the in-dialog re-offer; the
/// layer's own seed absorbs it below the router.
pub(super) fn names_server_branch(call: &Call, branch: &str) -> bool {
    if branch.is_empty() {
        return false;
    }
    std::iter::once(&call.a_leg)
        .chain(call.b_legs.iter())
        .flat_map(|leg| leg.dialogs.iter())
        .flat_map(|d| d.inbound_pending_requests.iter())
        .filter(|p| pending_invite(p))
        .any(|p| {
            pending_via(p).and_then(|v| v.branch().map(str::to_string)).as_deref() == Some(branch)
        })
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

/// The originator's top Via a pending relayed request was received with.
fn pending_via(p: &PendingRequest) -> Option<Via> {
    let top_via = p.source_vias.first()?;
    Via::parse(&SipStr::owned(top_via)).ok()
}

fn pending_invite(p: &PendingRequest) -> bool {
    p.method.eq_ignore_ascii_case("INVITE") && !p.cancelled
}

/// The client seed for an INVITE handle: the retained INVITE, the wire
/// destination it was sent to, and whether the record holds a provisional for
/// it. `None` for a handle whose bytes or destination do not read — nothing
/// can be rebuilt from it.
fn client_seed(handle: &InviteTxnHandle, provisional: bool) -> Option<TxnSeed> {
    let invite = parse_request(&handle.original_invite)?;
    let dest: SocketAddr =
        format!("{}:{}", handle.destination.host, handle.destination.port).parse().ok()?;
    Some(TxnSeed::ClientInvite { invite, dest, provisional })
}

/// The server seed for a relayed INVITE pending on one of `leg`'s dialogs:
/// keyed by the originator's top-Via branch and sent-by, matched on the originator's own
/// Call-ID and From-tag, answered under the To-tag its To already carries,
/// attributed to the originator's leg. The snapshot holds no request, so only
/// the INVITE's retransmissions are absorbed.
fn relayed_server_seed(call: &Call, leg: &Leg, pending: &PendingRequest) -> Option<TxnSeed> {
    let via = pending_via(pending)?;
    let branch = via.branch().filter(|b| !b.is_empty())?.to_string();
    let from_tag = header::From::parse(&SipStr::owned(&pending.source_from))
        .ok()?
        .tag()
        .unwrap_or_default()
        .to_string();
    let to_tag = header::To::parse(&SipStr::owned(&pending.source_to))
        .ok()
        .and_then(|to| to.tag().map(str::to_string));
    let originator = call::helpers::get_peer(call, &leg.leg_id)
        .map(str::to_string)
        .unwrap_or_else(|| call.a_leg.leg_id.clone());
    Some(TxnSeed::ServerInvite {
        branch,
        sent_by: SentBy::of(&via),
        call_id: pending.source_call_id.clone(),
        from_tag,
        to_tag,
        leg_id: Some(originator),
        original_request: None,
    })
}

/// A request re-parsed from the bytes an INVITE handle caches it as.
fn parse_request(bytes: &[u8]) -> Option<SipRequest> {
    match CustomParser::new().parse(bytes).ok()? {
        SipMessage::Request(r) => Some(r),
        SipMessage::Response(_) => None,
    }
}

#[cfg(test)]
mod tests;
