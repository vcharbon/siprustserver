//! Leg-level helpers: role resolution, leg lookup, the dialog of a leg named
//! by the one tag a caller does not already know, and termination-resolution
//! predicates. What a leg's INVITE and dialogs record as they end lives in
//! [`super::leg_end`].

use crate::model::{Call, CallModelState, Dialog, Leg, LegKind, LegState};

/// Resolve a leg's role, defaulting from `legId` when `kind` is absent.
pub fn leg_kind(leg: &Leg) -> LegKind {
    leg.kind.unwrap_or(if leg.leg_id == "a" { LegKind::A } else { LegKind::Destination })
}

/// Whether the generic relay / keepalive / failover rules own this leg.
/// `media` and an un-realigned `transfer-target` are unadopted; the explicit
/// `adopted` flag wins.
pub fn is_adopted(leg: &Leg) -> bool {
    if let Some(a) = leg.adopted {
        return a;
    }
    !matches!(leg_kind(leg), LegKind::Media | LegKind::TransferTarget)
}

/// The dialog of a leg this stack dialled whose peer tag is `peer_tag`: its
/// own tag is the From-tag of the leg's INVITE ([`Leg::outbound_invite`]).
pub fn callee_dialog<'a>(leg: &'a Leg, peer_tag: &str) -> Option<&'a Dialog> {
    let ours = &leg.outbound_invite.as_ref()?.our_tag;
    leg.dialogs.by_tags(ours, peer_tag)
}

/// The dialog of `leg` whose peer tag is `peer_tag`, on either face: on a leg
/// this stack dialled, its [`callee_dialog`]; on a leg whose INVITE it
/// received, where every dialog carries the caller's tag, the one the leg's
/// requests ride ([`crate::Dialogs::carrying`]) when it is that peer's.
pub fn dialog_with_peer<'a>(leg: &'a Leg, peer_tag: &str) -> Option<&'a Dialog> {
    if leg.outbound_invite.is_some() {
        return callee_dialog(leg, peer_tag);
    }
    leg.dialogs.carrying().filter(|d| d.peer_tag == peer_tag)
}

/// [`callee_dialog`], to update.
pub fn callee_dialog_mut<'a>(leg: &'a mut Leg, peer_tag: &str) -> Option<&'a mut Dialog> {
    let ours = leg.outbound_invite.as_ref()?.our_tag.clone();
    leg.dialogs.by_tags_mut(&ours, peer_tag)
}

/// This stack's tag on `leg` for a message no dialog names (RFC 3261
/// §8.2.6.2): the From-tag of the INVITE it sent the leg, else the tag of the
/// dialog it answers the leg's INVITE under ([`Leg::own_tag`]).
pub fn leg_own_tag(leg: &Leg) -> Option<&str> {
    leg.outbound_invite.as_ref().map(|i| i.our_tag.as_str()).or(leg.own_tag.as_deref())
}

/// The dialog a request this stack originates on `leg` rides: the leg's
/// confirmed dialog, else — before the leg's INVITE is answered — the dialog
/// it answers that INVITE under ([`Leg::own_tag`]).
pub fn carrying_dialog(leg: &Leg) -> Option<&Dialog> {
    leg.dialogs.confirmed().or_else(|| own_dialog(leg).filter(|d| d.is_live()))
}

/// The dialog `leg` answers its INVITE under ([`Leg::own_tag`]), in any
/// state; `None` on a leg this stack dialled, and before it holds a dialog.
pub fn own_dialog(leg: &Leg) -> Option<&Dialog> {
    let own = leg.own_tag.as_deref()?;
    leg.dialogs.iter().find(|d| d.our_tag == own)
}

/// The caller-facing dialog whose own tag is `our_tag`: its peer tag is the
/// caller's, the From-tag of her INVITE.
pub fn caller_dialog<'a>(call: &'a Call, our_tag: &str) -> Option<&'a Dialog> {
    call.a_leg.dialogs.by_tags(our_tag, &call.a_leg_invite.from_tag)
}

/// [`caller_dialog`], to update.
pub fn caller_dialog_mut<'a>(call: &'a mut Call, our_tag: &str) -> Option<&'a mut Dialog> {
    call.a_leg.dialogs.by_tags_mut(our_tag, &call.a_leg_invite.from_tag)
}

/// Whether the call waits on nothing more from `leg` before its record is
/// released ([`Leg::is_resolved`]). A leg whose CANCEL is in flight is not:
/// the callee owes a `487`, or a `200` may still cross the CANCEL (RFC 3261
/// §9.1), and finalization must not strand a ringing callee.
pub fn leg_is_resolved(leg: &Leg) -> bool {
    leg.is_resolved()
}

/// Whether a leg — or its whole call — is already going away, so an
/// asynchronous trigger (a timer fire, a transaction timeout, a failure
/// result) must drive NO forward progress through it: no failover consult,
/// no new final on a transaction that already carries one. True when the
/// call's lifecycle has entered `Terminating`/`Terminated`, or when the leg
/// itself reads `Terminated` (its CANCEL or BYE sent, its INVITE ended).
///
/// Distinct from [`leg_is_resolved`]: a `Cancelling` leg is *unresolved*
/// (its callee still owes a 487 or a crossing 200, so finalization waits) yet
/// already going away — resolution and progress-eligibility are different
/// questions.
pub fn leg_is_going_away(call_state: CallModelState, leg: &Leg) -> bool {
    matches!(call_state, CallModelState::Terminating | CallModelState::Terminated)
        || leg.state() == LegState::Terminated
}

/// Whether all legs of a terminating call have reached a terminal resolution
/// (see [`leg_is_resolved`]).
pub fn is_fully_resolved(call: &Call) -> bool {
    std::iter::once(&call.a_leg).chain(call.b_legs.iter()).all(leg_is_resolved)
}

/// Attach a b-leg without recording anything: the event-less attach for
/// fixtures and replica images. A leg this element originates goes through
/// [`super::add_originated_b_leg`], which records its INVITE.
pub fn add_b_leg(mut call: Call, leg: Leg) -> Call {
    call.b_legs.push(leg);
    call
}

/// Find a b-leg by legId.
pub fn find_b_leg<'a>(call: &'a Call, leg_id: &str) -> Option<&'a Leg> {
    call.b_legs.iter().find(|l| l.leg_id == leg_id)
}

/// Find any leg (a-leg or b-leg) by legId.
pub fn find_leg<'a>(call: &'a Call, leg_id: &str) -> Option<&'a Leg> {
    if call.a_leg.leg_id == leg_id {
        Some(&call.a_leg)
    } else {
        find_b_leg(call, leg_id)
    }
}

/// Find a b-leg by callId.
pub fn find_b_leg_by_call_id<'a>(call: &'a Call, call_id: &str) -> Option<&'a Leg> {
    call.b_legs.iter().find(|l| l.call_id == call_id)
}

/// Whether this stack answered the caller: true from the 2xx onward and still
/// true once the leg is `Terminated`. It reads the final the a-leg's initial
/// INVITE server transaction took (`invite_final_sent`, written at the one
/// a-facing final seam), not `LegState::Confirmed`, which a BYE ends.
/// One case turns it back to false: a 2xx the transaction layer's CANCEL 487
/// pre-empted before it reached the wire, which the record then states apart
/// ([`Leg::answer_preempted`]). Replication reconciliation reads the two
/// together ([`super::lifecycle_position`]), so its answer axis never moves
/// back.
pub fn caller_answered(call: &Call) -> bool {
    matches!(call.a_leg.invite_final_sent, Some(status) if (200..300).contains(&status))
}
