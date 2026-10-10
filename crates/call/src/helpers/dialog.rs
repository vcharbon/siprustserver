//! Dialog-level helpers: CSeq bookkeeping, the ACK-branch retention rule,
//! pending transparent-relay entries and the per-dialog SDP cache.

use crate::model::{Call, Dialog, DialogId, Leg, PendingRequest, RetainedEmission};

use super::identity::RequestTags;
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

/// Track the peer's latest CSeq on a dialog.
pub fn set_peer_cseq(call: Call, leg_id: &str, id: &DialogId, peer_cseq: i64) -> Call {
    update_dialog(call, leg_id, id, |d| d.peer_cseq = Some(peer_cseq))
}

/// Whether a request `leg_id`'s peer sent in the dialog its `tags` name carries
/// a CSeq below the last one that dialog took: out of order (RFC 3261 §12.2.2).
/// Each dialog is measured on its own sequence; a request naming no dialog is
/// not measured.
pub fn out_of_order(call: &Call, leg_id: &str, tags: RequestTags<'_>, cseq: i64) -> bool {
    find_leg(call, leg_id)
        .and_then(|leg| leg.dialogs.of_request(tags.to, tags.from))
        .and_then(|d| d.peer_cseq)
        .is_some_and(|last| cseq < last)
}

/// CSeq delta for a relayed request: `inbound - sourceRemoteCSeq`, clamped ≥ 1.
pub fn relay_cseq_delta(inbound_cseq: i64, source_remote_cseq: Option<i64>) -> i64 {
    match source_remote_cseq {
        None => 1,
        Some(s) => (inbound_cseq - s).max(1),
    }
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
/// still `Trying` / `Early` keeps it for the CANCEL.
pub fn close_rejected_invite_round(call: Call, leg_id: &str, branch: &str) -> Call {
    update_leg(call, leg_id, |leg| {
        if !matches!(leg.state, crate::model::LegState::Confirmed) {
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
/// §14.1): a transparently relayed INVITE awaiting its final response
/// (rule 1), or a 2xx un-ACKed on either face — one this side sent (a
/// re-INVITE's or the call's own answer) or one taken here whose ACK has not
/// left yet (rule 2; RFC 6026 names the interval *Accepted*). While any holds,
/// a new INVITE on the dialog is glare and is refused, 491 or 500 by which
/// INVITE is open (RFC 3261 §14.2).
pub fn invite_transaction_open(dialog: &Dialog) -> bool {
    dialog.inbound_pending_requests.iter().any(|p| p.method.eq_ignore_ascii_case("INVITE"))
        || dialog.pending_reinvite_2xx.is_some()
        || dialog.answered_2xx.is_some()
        || dialog.awaited_ack_cseq.is_some()
}

/// Whether an INVITE from `source_leg_id`'s peer, carrying `tags`,
/// meets glare (RFC 3261 §14.1): an INVITE transaction still open
/// ([`invite_transaction_open`]) on the dialog it arrived on or on the dialog
/// its relay would be regenerated on ([`relay_peer_dialog`]).
pub fn invite_glare(call: &Call, source_leg_id: &str, tags: RequestTags<'_>) -> bool {
    let source =
        find_leg(call, source_leg_id).and_then(|leg| leg.dialogs.of_request(tags.to, tags.from));
    source.is_some_and(invite_transaction_open)
        || relay_peer_dialog(call, source_leg_id, tags)
            .is_some_and(|(_, d)| invite_transaction_open(d))
}

/// Whether `source_leg_id`'s peer, sending an INVITE carrying `tags`, has an
/// earlier INVITE of its own on the dialog that this side has sent no final
/// to (RFC 3261 §14.2, first clause): its relay awaits a final on the
/// relay-target dialog and was not CANCELled.
pub fn sender_invite_unanswered(call: &Call, source_leg_id: &str, tags: RequestTags<'_>) -> bool {
    relay_peer_dialog(call, source_leg_id, tags).is_some_and(|(_, d)| {
        d.inbound_pending_requests
            .iter()
            .any(|p| p.method.eq_ignore_ascii_case("INVITE") && !p.cancelled)
    })
}

/// Whether `source_leg_id`'s peer has an earlier INVITE of its own whose 2xx
/// this side sent and still awaits the ACK for, on the dialog `tags` name
/// (RFC 6026 *Accepted*).
pub fn sender_invite_unacknowledged(
    call: &Call,
    source_leg_id: &str,
    tags: RequestTags<'_>,
) -> bool {
    find_leg(call, source_leg_id)
        .and_then(|leg| leg.dialogs.of_request(tags.to, tags.from))
        .is_some_and(|d| d.pending_reinvite_2xx.is_some() || d.answered_2xx.is_some())
}

/// Whether an UPDATE that `source_leg_id`'s peer sent is still awaiting its
/// final (RFC 3311 §5.2): its relay snapshot is on the dialog a request from
/// that leg, carrying `tags`, is relayed on.
pub fn peer_update_pending(call: &Call, source_leg_id: &str, tags: RequestTags<'_>) -> bool {
    relay_peer_dialog(call, source_leg_id, tags).is_some_and(|(_, d)| {
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
