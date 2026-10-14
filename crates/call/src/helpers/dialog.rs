//! Dialog-level helpers: CSeq bookkeeping, the ACK-branch retention rule,
//! pending transparent-relay entries, a retired dialog's relays and release,
//! and the per-dialog SDP cache.

use crate::model::{
    Call, Dialog, DialogId, DialogState, Leg, PendingRequest, RetainedEmission, RetiredDialog,
};

use super::leg::find_leg;
use super::lens::{update_dialog, update_leg};
use super::peering::relay_peer_dialog;

// ── CSeq ────────────────────────────────────────────────────────────────────

/// Bump a dialog's own CSeq by `delta` (CSeq is dialog-scoped — §12.2.1.1).
/// Each dialog (including each forked early dialog) owns an independent
/// sequence, so a sibling fork's CSeq never constrains this one.
pub fn bump_our_cseq(call: Call, leg_id: &str, id: &DialogId, delta: i64) -> Call {
    update_dialog(call, leg_id, id, |d| d.our_cseq += delta)
}

/// Note a request the peer sent in dialog `id` as in order: its CSeq becomes
/// the dialog's remote sequence number unless that already stands higher
/// (RFC 3261 §12.2.2). The one writer of `peer_cseq` past the dialog's
/// creation.
pub fn advance_peer_cseq(call: Call, leg_id: &str, id: &DialogId, cseq: i64) -> Call {
    update_dialog(call, leg_id, id, |d| {
        d.peer_cseq = Some(d.peer_cseq.map_or(cseq, |c| c.max(cseq)))
    })
}

// ── ACK branch ──────────────────────────────────────────────────────────────

/// Retain the Via branch of the ACK-for-2xx just sent on `leg_id`'s
/// established dialog, so a
/// re-ACK of a **retransmitted** 2xx reuses the SAME branch (RFC 3261 §13.2.2.4).
/// A fresh branch would mint a new client transaction and never quiesce the
/// answerer's INVITE server txn, leaking / late-timing-out the confirmed call
/// when the first ACK is lost. Idempotent: whoever writes it first wins — the
/// 2xx that armed it on confirmation, else the first ACK — and every later ACK
/// re-observes that value. Reset to `None` wherever a new INVITE transaction is
/// cached on the dialog, so the stored branch always belongs to the current
/// INVITE's CSeq.
pub fn retain_ack_branch(call: Call, leg_id: &str, branch: &str) -> Call {
    update_leg(call, leg_id, |leg| {
        if let Some(d) = leg.dialogs.established_mut() {
            if d.ack_branch.is_none() {
                d.ack_branch = Some(branch.to_string());
            }
        }
    })
}

/// Retain the ACK-for-2xx datagram just emitted on `leg_id`'s established
/// dialog, so a
/// re-ACK of a **retransmitted** 2xx re-passes THE ACK to the transport
/// (RFC 3261 §13.2.2.4) instead of composing an equivalent — on a delayed offer
/// the retained bytes carry the answer a fresh composition would drop
/// (RFC 3264 §4). First write wins, like `retain_ack_branch`: the ACK the 2xx
/// triggered is the one every later copy re-sends. Reset to `None` alongside
/// `ack_branch` wherever a new INVITE transaction is cached on the dialog.
pub fn retain_emitted_ack(call: Call, leg_id: &str, ack: RetainedEmission) -> Call {
    update_leg(call, leg_id, |leg| {
        if let Some(d) = leg.dialogs.established_mut() {
            if d.emitted_ack.is_none() {
                d.emitted_ack = Some(ack.clone());
            }
        }
    })
}

/// Forget the INVITE client-transaction handle on `leg_id`'s dialog whose
/// branch a non-2xx final answered (RFC 3261 §17.1.1.3: the transaction ends
/// with that final, hop-ACKed below the TU), so the record names nothing in
/// flight for it. Only an open round is closed: a handle kept for a taken 2xx
/// (`ack_branch` / `awaited_ack_cseq` set) stays for the ACK and its re-ACKs,
/// whatever a later final on the same branch says; the initial INVITE of a leg
/// holding no confirmed dialog keeps it for the CANCEL.
pub fn close_rejected_invite_round(call: Call, leg_id: &str, branch: &str) -> Call {
    update_leg(call, leg_id, |leg| {
        if leg.dialogs.confirmed().is_none() {
            return;
        }
        for d in leg.dialogs.iter_mut() {
            let round_open = d.ack_branch.is_none() && d.awaited_ack_cseq.is_none();
            if round_open && d.pending_invite_txn.as_ref().is_some_and(|h| h.branch == branch) {
                d.pending_invite_txn = None;
            }
        }
    })
}

/// Arm (`Some`) or discharge (`None`) the §13.2.2.4 relay-ACK obligation on
/// `leg_id`'s established dialog. `cseq` is the number the acknowledging peer's
/// ACK will carry — the peer's own INVITE CSeq, not this dialog's.
pub fn set_awaited_ack_cseq(call: Call, leg_id: &str, cseq: Option<i64>) -> Call {
    update_leg(call, leg_id, |leg| {
        if let Some(d) = leg.dialogs.established_mut() {
            d.awaited_ack_cseq = cseq;
        }
    })
}

// ── Pending transparent-relay requests ──────────────────────────────────────

/// Add a pending transparent-relay entry to a dialog.
pub fn add_pending_request(call: Call, leg_id: &str, id: &DialogId, entry: PendingRequest) -> Call {
    update_dialog(call, leg_id, id, |d| d.inbound_pending_requests.push(entry))
}

/// Whether an INVITE transaction is still open on this dialog (RFC 3261
/// §14.1): a transparently relayed INVITE awaiting its final response, or an
/// INVITE this side sent on the dialog, relayed or its own, with no 2xx taken
/// (rule 1), or a 2xx un-ACKed on either face — one this side sent (a
/// re-INVITE's or the call's own answer) or one taken here whose ACK has not
/// left yet (rule 2; RFC 6026 names the interval *Accepted*). While any
/// holds, a new INVITE on the dialog is glare and is refused, 491 or 500 by
/// which INVITE is open (RFC 3261 §14.2).
pub fn invite_transaction_open(dialog: &Dialog) -> bool {
    dialog.inbound_pending_requests.iter().any(|p| p.method.eq_ignore_ascii_case("INVITE"))
        || own_invite_untaken(dialog)
        || dialog.pending_reinvite_2xx.is_some()
        || dialog.answered_2xx.is_some()
        || dialog.awaited_ack_cseq.is_some()
}

/// Whether the INVITE client transaction cached on the dialog has no 2xx
/// taken: a 2xx arms its ACK (`ack_branch`) or the ACK the peer owes
/// (`awaited_ack_cseq`). A non-2xx final forgets the transaction on a leg
/// holding a confirmed dialog ([`close_rejected_invite_round`]); on a leg
/// holding none it keeps it and ends the leg's early dialogs instead.
fn own_invite_untaken(dialog: &Dialog) -> bool {
    dialog.pending_invite_txn.is_some()
        && dialog.ack_branch.is_none()
        && dialog.awaited_ack_cseq.is_none()
}

/// Whether an INVITE from `source_leg_id`'s peer, on its dialog `source`,
/// meets glare (RFC 3261 §14.1): an INVITE transaction still open
/// ([`invite_transaction_open`]) on that dialog or on the dialog its relay
/// would be regenerated on ([`relay_peer_dialog`]), or the dialog-creating
/// INVITE of either leg still without a final: the sender's own, or the relay
/// target's, whose early dialog the relay would ride.
pub fn invite_glare(call: &Call, source_leg_id: &str, source: Option<&DialogId>) -> bool {
    let named = named_dialog(call, source_leg_id, source);
    named.is_some_and(invite_transaction_open)
        || (named.is_some() && source_initial_invite_unanswered(call, source_leg_id))
        || relay_peer_dialog(call, source_leg_id, source).is_some_and(|(leg, d)| {
            invite_transaction_open(d) || initial_invite_unanswered(call, leg)
        })
}

/// Whether `leg` is the originator's leg and the dialog-creating INVITE this
/// stack received on it has no final sent yet: an INVITE on one of its early
/// dialogs is a second INVITE before the first's final (RFC 3261 §14.2,
/// first clause).
fn initial_invite_unanswered(call: &Call, leg: &Leg) -> bool {
    leg.leg_id == call.a_leg.leg_id && leg.invite_final_sent.is_none()
}

/// [`initial_invite_unanswered`] for the leg named `leg_id`.
fn source_initial_invite_unanswered(call: &Call, leg_id: &str) -> bool {
    find_leg(call, leg_id).is_some_and(|leg| initial_invite_unanswered(call, leg))
}

/// The dialog of `leg_id` that `id` names.
fn named_dialog<'a>(call: &'a Call, leg_id: &str, id: Option<&DialogId>) -> Option<&'a Dialog> {
    let id = id?;
    find_leg(call, leg_id)?.dialogs.by_tags(&id.our_tag, &id.peer_tag)
}

/// Whether `source_leg_id`'s peer, sending an INVITE on its dialog `source`,
/// has an earlier INVITE of its own on the dialog that this side has sent no final
/// to (RFC 3261 §14.2, first clause): its relay awaits a final on the
/// relay-target dialog and was not CANCELled, or the dialog is an early one
/// of its dialog-creating INVITE, still without a final.
pub fn sender_invite_unanswered(
    call: &Call,
    source_leg_id: &str,
    source: Option<&DialogId>,
) -> bool {
    let relayed = relay_peer_dialog(call, source_leg_id, source).is_some_and(|(_, d)| {
        d.inbound_pending_requests
            .iter()
            .any(|p| p.method.eq_ignore_ascii_case("INVITE") && !p.cancelled)
    });
    relayed
        || (named_dialog(call, source_leg_id, source).is_some()
            && source_initial_invite_unanswered(call, source_leg_id))
}

/// Whether `source_leg_id`'s peer has an earlier INVITE of its own whose 2xx
/// this side sent and still awaits the ACK for, on its dialog `source`
/// (RFC 6026 *Accepted*).
pub fn sender_invite_unacknowledged(
    call: &Call,
    source_leg_id: &str,
    source: Option<&DialogId>,
) -> bool {
    named_dialog(call, source_leg_id, source)
        .is_some_and(|d| d.pending_reinvite_2xx.is_some() || d.answered_2xx.is_some())
}

/// Whether an UPDATE that `source_leg_id`'s peer sent is still awaiting its
/// final (RFC 3311 §5.2): its relay snapshot is on the dialog a request from
/// that leg's dialog `source` is relayed on.
pub fn peer_update_pending(call: &Call, source_leg_id: &str, source: Option<&DialogId>) -> bool {
    relay_peer_dialog(call, source_leg_id, source).is_some_and(|(_, d)| {
        d.inbound_pending_requests.iter().any(|p| p.method.eq_ignore_ascii_case("UPDATE"))
    })
}

/// Where the relay a response answers is kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingHolder {
    /// Among the pending relays of the leg's dialog with this identity.
    Dialog(DialogId),
    /// Among the open relays of the leg's retired dialog with this identity.
    Retired(DialogId),
}

/// The relay a response arriving on `leg_id` with outbound CSeq `cseq`
/// answers (RFC 3261 §8.1.3.3), and where it is kept: on the dialog the
/// response's tags name, or among that dialog's open relays once it retired.
/// A response with no To-tag reads the leg's confirmed dialog. Never another
/// dialog's relay: two dialogs may carry the same CSeq.
pub fn relayed_pending(
    call: &Call,
    leg_id: &str,
    to: Option<&str>,
    from: Option<&str>,
    cseq: i64,
) -> Option<(PendingHolder, PendingRequest)> {
    let leg = find_leg(call, leg_id)?;
    let d = match to {
        None => leg.dialogs.confirmed()?,
        Some(_) => leg.dialogs.of_response(to, from)?,
    };
    match d.retired() {
        Some(r) => r
            .open_relays
            .iter()
            .find(|p| p.outbound_cseq == cseq)
            .map(|p| (PendingHolder::Retired(d.id()), p.clone())),
        None => find_pending_request(d, cseq).map(|p| (PendingHolder::Dialog(d.id()), p.clone())),
    }
}

/// Close the relay `holder` keeps with outbound CSeq `cseq` on `leg_id`: its
/// transaction has its final.
pub fn close_relay(call: Call, leg_id: &str, holder: &PendingHolder, cseq: i64) -> Call {
    match holder {
        PendingHolder::Dialog(id) => remove_pending_request(call, leg_id, id, cseq),
        PendingHolder::Retired(id) => update_dialog(call, leg_id, id, |d| {
            if let Some(r) = d.retired_mut() {
                r.open_relays.retain(|p| p.outbound_cseq != cseq);
            }
        }),
    }
}

/// Close every relay still open in `leg`'s retired dialogs and return them,
/// each with its dialog: the leg ends, so no final reaches them but the one
/// this stack answers (RFC 3261 §8.2.6).
pub fn take_retired_relays(leg: &mut Leg) -> Vec<(DialogId, PendingRequest)> {
    let mut taken = Vec::new();
    for d in leg.dialogs.iter_mut() {
        let id = d.id();
        if let Some(r) = d.retired_mut() {
            taken.extend(std::mem::take(&mut r.open_relays).into_iter().map(|p| (id.clone(), p)));
        }
    }
    taken
}

/// How many relays are still open in `leg`'s retired dialogs.
pub fn retired_relay_count(leg: &Leg) -> usize {
    leg.dialogs.iter().filter_map(Dialog::retired).map(|r| r.open_relays.len()).sum()
}

/// Find a pending transparent-relay entry by outbound CSeq.
pub fn find_pending_request(dialog: &Dialog, outbound_cseq: i64) -> Option<&PendingRequest> {
    dialog.inbound_pending_requests.iter().find(|p| p.outbound_cseq == outbound_cseq)
}

/// Mark a pending transparent-relay entry CANCELled (RFC 3261 §9): the relayed
/// (re-)INVITE it snapshots has been CANCELled toward its target, so the
/// eventual final response resolves locally instead of being relayed back.
pub fn cancel_pending_request(call: Call, leg_id: &str, id: &DialogId, outbound_cseq: i64) -> Call {
    update_dialog(call, leg_id, id, |d| {
        if let Some(p) =
            d.inbound_pending_requests.iter_mut().find(|p| p.outbound_cseq == outbound_cseq)
        {
            p.cancelled = true;
        }
    })
}

/// Remove a pending transparent-relay entry after its response is handled.
pub fn remove_pending_request(call: Call, leg_id: &str, id: &DialogId, outbound_cseq: i64) -> Call {
    update_dialog(call, leg_id, id, |d| {
        d.inbound_pending_requests.retain(|p| p.outbound_cseq != outbound_cseq)
    })
}

// ── Per-dialog SDP cache ────────────────────────────────────────────────────

/// Cache an SDP body on the callee dialog of `leg_id` whose peer tag is
/// `b_tag`, and on no other: under downstream forking each early dialog
/// carries its own answer (RFC 3264 §4). A miss is a no-op — the executor
/// ensures the dialog exists before caching.
pub fn cache_sdp_on_leg_dialog(call: Call, leg_id: &str, b_tag: &str, body: Vec<u8>) -> Call {
    update_leg(call, leg_id, |leg| {
        if let Some(d) = super::leg::callee_dialog_mut(leg, b_tag) {
            d.cached_sdp = Some(body);
        }
    })
}

/// The SDP cached on the callee dialog of `leg_id` whose peer tag is `b_tag`
/// — never another fork's.
pub fn cached_sdp_for_leg_dialog<'a>(
    call: &'a Call,
    leg_id: &str,
    b_tag: &str,
) -> Option<&'a [u8]> {
    let leg = find_leg(call, leg_id)?;
    super::leg::callee_dialog(leg, b_tag)?.cached_sdp.as_deref()
}

/// Note the final or the timeout of the BYE on `bye_branch` that released a
/// retired dialog (RFC 3261 §15.1.1): that release is settled.
pub fn settle_straggler_release(mut call: Call, bye_branch: &str) -> Call {
    let legs = std::iter::once(&mut call.a_leg).chain(call.b_legs.iter_mut());
    for d in legs.flat_map(|l| l.dialogs.iter_mut()) {
        if let DialogState::Retired(RetiredDialog { release: Some(r), .. }) = &mut d.state {
            if r.bye_branch == bye_branch {
                r.settled = true;
            }
        }
    }
    call
}
