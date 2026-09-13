//! Leg-level helpers: role resolution, leg/dialog lookup, state and
//! disposition setters, termination-resolution predicates, and the per-leg
//! tag readers.

use crate::model::{
    ByeDisposition, Call, CallModelState, Dialog, Leg, LegDisposition, LegKind, LegState,
};

use super::lens::update_leg;

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

/// Find a dialog by remote tag (early-state forking on the b-leg).
pub fn find_dialog_by_to_tag<'a>(leg: &'a Leg, to_tag: &str) -> Option<&'a Dialog> {
    leg.dialogs.iter().find(|d| d.sip.remote_tag == to_tag)
}

/// The single confirmed dialog (only valid when `leg.state == Confirmed`).
pub fn confirmed_dialog(leg: &Leg) -> Option<&Dialog> {
    leg.dialogs.first()
}

/// Set the state of a specific leg.
pub fn set_leg_state(call: Call, leg_id: &str, state: LegState) -> Call {
    update_leg(call, leg_id, |l| l.state = state)
}

/// Set the disposition of a specific leg.
pub fn set_leg_disposition(call: Call, leg_id: &str, disposition: LegDisposition) -> Call {
    update_leg(call, leg_id, |l| l.disposition = disposition)
}

/// Set the BYE disposition of a specific leg.
pub fn set_bye_disposition(call: Call, leg_id: &str, bye: ByeDisposition) -> Call {
    update_leg(call, leg_id, |l| l.bye_disposition = Some(bye))
}

/// Record the final sent on a leg's initial inbound INVITE
/// ([`Leg::invite_final_sent`]). The first final stands: a leg that already
/// carries one is left as is — the transaction took exactly that one final.
pub fn record_invite_final(call: Call, leg_id: &str, status: u16) -> Call {
    update_leg(call, leg_id, |l| {
        l.invite_final_sent.get_or_insert(status);
    })
}

/// Whether a single leg has reached a terminal resolution for termination
/// bookkeeping. A `trying` leg with no `byeDisposition` never established, so it
/// is considered resolved.
///
/// A leg still in `Cancelling` disposition is **not** resolved even though its
/// interim `Cancelled` bye disposition reads terminal: an internal CANCEL is in
/// flight and its transaction has not settled — the callee owes us a `487`, or a
/// `200 OK` may still be crossing our CANCEL on the wire (RFC 3261 §9.1). Holding
/// the leg unresolved keeps the call alive until `resolve-cancel-response` (the
/// 487) or `cancel-200-crossing` (200 → ACK + BYE) reaps the abandoned callee, so
/// finalization (RemoveCall) never strands a ringing b-leg. Both of those rules —
/// and the force-terminal reaper/safety paths — clear the `Cancelling`
/// disposition as they resolve the leg, so this stays unresolved only for the
/// duration of the in-flight CANCEL (bounded by the terminating safety timer).
pub fn leg_is_resolved(leg: &Leg) -> bool {
    if leg.disposition == LegDisposition::Cancelling {
        return false;
    }
    match leg.bye_disposition {
        None => leg.state == LegState::Trying,
        Some(b) => b.is_terminal(),
    }
}

/// Whether a leg — or its whole call — is already going away, so an
/// asynchronous trigger (a timer fire, a transaction timeout, a failure
/// result) must drive NO forward progress through it: no failover consult,
/// no new final on a transaction that already carries one. True when the
/// call's lifecycle has entered `Terminating`/`Terminated`, when the leg's
/// CANCEL is in flight (`Cancelling` — the leg's state still reads `Trying`,
/// so state alone cannot discriminate), or when the leg itself is
/// `Terminated`.
///
/// Distinct from [`leg_is_resolved`]: a `Cancelling` leg is *unresolved*
/// (its callee still owes a 487 or a crossing 200, so finalization waits) yet
/// already going away — resolution and progress-eligibility are different
/// questions.
pub fn leg_is_going_away(call_state: CallModelState, leg: &Leg) -> bool {
    matches!(call_state, CallModelState::Terminating | CallModelState::Terminated)
        || leg.disposition == LegDisposition::Cancelling
        || leg.state == LegState::Terminated
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

/// The B2BUA's own tag for a leg (`sip.localTag` of `dialogs[0]`).
pub fn b2bua_tag(call: &Call, leg_id: &str) -> Option<String> {
    if leg_id == "a" {
        return call.a_leg.dialogs.first().map(|d| d.sip.local_tag.clone());
    }
    let b = find_b_leg(call, leg_id)?;
    Some(b.dialogs.first().map(|d| d.sip.local_tag.clone()).unwrap_or_else(|| b.from_tag.clone()))
}

/// Whether `tag` is one this stack put on `leg_id`: the `localTag` of any of
/// its dialogs, an a-facing fork tag in the tag map, or the From-tag the leg's
/// own INVITE carried (a b-leg). `None` while the leg holds no tag of its own,
/// so a request's To-tag has nothing to be measured against (RFC 3261 §12.2.2).
pub fn holds_local_tag(call: &Call, leg_id: &str, tag: &str) -> Option<bool> {
    let leg = find_leg(call, leg_id)?;
    let mut own: Vec<&str> =
        leg.dialogs.iter().map(|d| d.sip.local_tag.as_str()).filter(|t| !t.is_empty()).collect();
    if leg_id == "a" {
        own.extend(call.tag_map.iter().map(|m| m.a_tag.as_str()).filter(|t| !t.is_empty()));
    } else if !leg.from_tag.is_empty() {
        own.push(leg.from_tag.as_str());
    }
    (!own.is_empty()).then(|| own.contains(&tag))
}

/// The remote party's tag for a leg (`sip.remoteTag` of `dialogs[0]`).
pub fn remote_tag(call: &Call, leg_id: &str) -> Option<String> {
    if leg_id == "a" {
        return Some(
            call.a_leg
                .dialogs
                .first()
                .map(|d| d.sip.remote_tag.clone())
                .unwrap_or_else(|| call.a_leg.from_tag.clone()),
        );
    }
    let b = find_b_leg(call, leg_id)?;
    b.dialogs.first().map(|d| d.sip.remote_tag.clone())
}
