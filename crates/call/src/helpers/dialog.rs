//! Dialog-level helpers: CSeq bookkeeping, INVITE glare and the per-dialog
//! SDP cache. The request records a dialog holds are
//! [`super::request_records`]' and [`super::invite_records`]'.

use crate::model::{Call, Dialog, DialogId, Leg};

use super::invite_records::{awaited_ack_cseq, invite_untaken, relayed_invite_pending};
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

// ── INVITE glare ────────────────────────────────────────────────────────────

/// Whether an INVITE transaction is still open on this dialog (RFC 3261
/// §14.1): an INVITE this side sent on the dialog, relayed or its own, with
/// no 2xx taken (rule 1), or a 2xx un-ACKed on either face — one this side
/// sent (a re-INVITE's or the call's own answer) or one taken here whose ACK
/// has not left yet (rule 2; RFC 6026 names the interval *Accepted*). While
/// any holds, a new INVITE on the dialog is glare and is refused, 491 or 500
/// by which INVITE is open (RFC 3261 §14.2). A non-2xx final closes a
/// re-INVITE ([`super::close_rejected_invite_round`]); on a leg holding no
/// confirmed dialog the dialling INVITE stays, and its early dialogs end.
pub fn invite_transaction_open(dialog: &Dialog) -> bool {
    invite_untaken(dialog) || dialog.answered_invite.is_some() || awaited_ack_cseq(dialog).is_some()
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
    leg.leg_id == call.a_leg.leg_id && leg.invite_final_sent().is_none()
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
    let relayed = relay_peer_dialog(call, source_leg_id, source)
        .is_some_and(|(_, d)| relayed_invite_pending(d));
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
    named_dialog(call, source_leg_id, source).is_some_and(|d| d.answered_invite.is_some())
}

/// Whether an UPDATE that `source_leg_id`'s peer sent is still awaiting its
/// final (RFC 3311 §5.2): its relay record is on the dialog a request from
/// that leg's dialog `source` is relayed on.
pub fn peer_update_pending(call: &Call, source_leg_id: &str, source: Option<&DialogId>) -> bool {
    relay_peer_dialog(call, source_leg_id, source)
        .is_some_and(|(_, d)| d.requests.iter().any(|p| p.is_relay() && p.method == "UPDATE"))
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
