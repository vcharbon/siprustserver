//! Which leg talks to which: the caller-facing dialog that fronts a callee
//! dialog, the active peer pair (INAP-style split/merge), and
//! transparent-relay peer resolution.

use crate::model::{ActivePeer, Call, Dialog, DialogId, Leg, LegState};

use super::identity::RequestTags;
use super::leg::{callee_dialog, caller_dialog, find_b_leg, is_adopted};

// ── Caller-facing fronts ────────────────────────────────────────────────────

/// The caller-facing dialog that fronts `b_leg_id`'s dialog with peer tag
/// `b_tag` ([`Dialog::fronts`]), in any state.
pub fn fronting_dialog<'a>(call: &'a Call, b_leg_id: &str, b_tag: &str) -> Option<&'a Dialog> {
    call.a_leg.dialogs.fronting(b_leg_id, b_tag)
}

/// Whether a provisional this stack sent the caller carried `a_tag`: the
/// caller-facing dialog of that tag is marked shown ([`Dialog::shown`]).
pub fn a_tag_shown(call: &Call, a_tag: &str) -> bool {
    caller_dialog(call, a_tag).is_some_and(|d| d.shown)
}

// ── Active peer (INAP-style split/merge) ────────────────────────────────────

/// The peer leg for routing, or `None` if `leg_id` is not peered.
pub fn get_peer<'a>(call: &'a Call, leg_id: &str) -> Option<&'a str> {
    let p = call.active_peer.as_ref()?;
    if p.leg_a == leg_id {
        Some(&p.leg_b)
    } else if p.leg_b == leg_id {
        Some(&p.leg_a)
    } else {
        None
    }
}

/// Connect two legs (INAP MergeCallSegments) — replaces any existing pairing.
pub fn merge_leg(mut call: Call, leg_a: impl Into<String>, leg_b: impl Into<String>) -> Call {
    call.active_peer = Some(ActivePeer { leg_a: leg_a.into(), leg_b: leg_b.into() });
    call
}

/// Disconnect a leg from its peer (INAP SplitLeg) if it is in the current pair.
pub fn split_leg(mut call: Call, leg_id: &str) -> Call {
    if let Some(p) = &call.active_peer {
        if p.leg_a == leg_id || p.leg_b == leg_id {
            call.active_peer = None;
        }
    }
    call
}

/// All leg IDs that currently have a peer.
pub fn all_peered_legs(call: &Call) -> Vec<String> {
    match &call.active_peer {
        None => Vec::new(),
        Some(p) => vec![p.leg_a.clone(), p.leg_b.clone()],
    }
}

// ── Transparent-relay peer resolution ───────────────────────────────────────

/// Resolve the leg a transparent `RelayToPeer` from `source_leg_id` targets,
/// plus the target dialog when the source's dialog names one (forking: the
/// callee early dialog toward a b-leg, the caller-facing dialog toward the
/// a-leg). The ONE resolver both the executor's relay path and the
/// rule-vocabulary readiness predicate ([`relay_peer_dialog_ready`]) share —
/// keep them in lockstep by construction.
///
/// Order:
///   1. the active pair (post-merge) — the merge's `active_peer` names the
///      live leg, whatever earlier dialog fronted the caller's;
///   2. pre-merge a-leg in-dialog request (forking PRACK/UPDATE): the
///      caller-facing dialog its tags name resolves, through what it fronts,
///      to the right b-leg + callee fork;
///   3. implicit b→a fallback, gated on leg adoption (ADR-0014): an unadopted
///      leg (parked `media`, un-realigned `transfer-target`) is owned by its
///      service rule and must never be mis-routed to A;
///   4. fallback pairing: a-leg ↔ (first confirmed b-leg, else first b-leg).
///
/// Toward the a-leg, the target dialog is the live caller-facing one fronting
/// the source's dialog (named by its From-tag); one the answer retired names
/// none, and the dialog the a-leg's requests ride carries the request.
pub fn resolve_relay_peer(
    call: &Call,
    source_leg_id: &str,
    tags: RequestTags<'_>,
) -> (Option<String>, Option<DialogId>) {
    let a_face = || {
        let face = tags.from.and_then(|t| fronting_dialog(call, source_leg_id, t));
        face.filter(|d| d.is_live()).map(Dialog::id)
    };
    let a_leg_id = &call.a_leg.leg_id;
    if let Some(p) = &call.active_peer {
        if p.leg_a == source_leg_id {
            let face = (&p.leg_b == a_leg_id).then(a_face).flatten();
            return (Some(p.leg_b.clone()), face);
        }
        if p.leg_b == source_leg_id {
            let face = (&p.leg_a == a_leg_id).then(a_face).flatten();
            return (Some(p.leg_a.clone()), face);
        }
    }
    if source_leg_id == a_leg_id {
        if let Some(face) = call.a_leg.dialogs.of_request(tags.to, tags.from) {
            if let Some((leg_id, fork)) = fronted_fork(call, face) {
                return (Some(leg_id), fork.map(Dialog::id));
            }
        }
    } else if let Some(leg) = find_b_leg(call, source_leg_id) {
        if !is_adopted(leg) {
            return (None, None);
        }
    }
    if source_leg_id == a_leg_id {
        let peer = call
            .b_legs
            .iter()
            .find(|l| l.state == LegState::Confirmed)
            .or_else(|| call.b_legs.first())
            .map(|l| l.leg_id.clone());
        return (peer, None);
    }
    (Some(a_leg_id.clone()), a_face())
}

/// The callee leg and dialog the caller-facing dialog `face` relays: the
/// first it fronts whose dialog is live, else the first it fronts. `None`
/// when it fronts none.
fn fronted_fork<'a>(call: &'a Call, face: &Dialog) -> Option<(String, Option<&'a Dialog>)> {
    let fork = |f: &crate::model::PeerDialog| {
        find_b_leg(call, &f.leg_id).and_then(|l| callee_dialog(l, &f.peer_tag))
    };
    let first = face.fronts.first()?;
    face.fronts
        .iter()
        .find_map(|f| fork(f).filter(|d| d.is_live()).map(|d| (f.leg_id.clone(), Some(d))))
        .or_else(|| Some((first.leg_id.clone(), fork(first))))
}

/// Is the relay target of an in-dialog request from `source_leg_id` in a
/// **relayable** state? `false` exactly when a `RelayToPeer` would go nowhere
/// useful: no peer leg resolves, the peer leg is `Terminated` (a failed b-leg
/// whose `/call/failure` reroute is still pending), or no target dialog
/// resolves on it (a replacement leg still `Trying`). An `Early` dialog is
/// relayable — an early-dialog UPDATE is the RFC 3311 §5.1 normal case.
pub fn relay_peer_dialog_ready(call: &Call, source_leg_id: &str, tags: RequestTags<'_>) -> bool {
    relay_peer_dialog(call, source_leg_id, tags)
        .is_some_and(|(leg, _)| leg.state != LegState::Terminated)
}

/// The `(leg, dialog)` a relayed in-dialog request from `source_leg_id` would
/// be regenerated on: [`resolve_relay_peer`]'s leg pick, then the dialog it
/// names, else the one the leg's requests ride ([`crate::Dialogs::carrying`]). The single resolver behind both
/// [`relay_peer_dialog_ready`] and the rule-vocabulary peer-dialog reads, so
/// match and action never disagree.
pub fn relay_peer_dialog<'a>(
    call: &'a Call,
    source_leg_id: &str,
    tags: RequestTags<'_>,
) -> Option<(&'a Leg, &'a Dialog)> {
    let (peer, target) = resolve_relay_peer(call, source_leg_id, tags);
    let peer_id = peer?;
    let leg = if peer_id == call.a_leg.leg_id { &call.a_leg } else { find_b_leg(call, &peer_id)? };
    let dialog = match target {
        Some(id) => leg.dialogs.by_tags(&id.our_tag, &id.peer_tag)?,
        None => leg.dialogs.carrying()?,
    };
    Some((leg, dialog))
}
