//! Which leg talks to which: the caller-facing dialog that fronts a callee
//! dialog, the active peer pair (INAP-style split/merge), and
//! transparent-relay peer resolution.

use crate::model::{ActivePeer, Call, Dialog, DialogId, Leg, LegState};

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

/// Where a transparent relay of an in-dialog request from a leg goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayPeer {
    /// The peer leg, and the dialog of it the request continues on; `None`
    /// when no dialog of the peer leg can carry it now: the leg holds none
    /// yet, several early dialogs and none named, or the caller-facing
    /// dialog it would ride was never shown to the caller.
    Leg { leg_id: String, dialog: Option<DialogId> },
    /// No peer leg: a caller request while the call holds no b-leg.
    NoLeg,
    /// The source leg is a service's own (ADR-0014): no relay is made for it.
    Unadopted,
}

/// Resolve the leg and dialog a transparent `RelayToPeer` from
/// `source_leg_id`'s dialog `source` continues on. The ONE
/// resolver the executor's relay path, its finals and the rule-vocabulary
/// readiness predicate ([`relay_peer_dialog_ready`]) share.
///
/// Order:
///   1. the active pair (post-merge): the merge names the live leg;
///   2. a caller request: the callee dialog its caller-facing dialog fronts;
///   3. an unadopted leg (parked `media`, un-realigned `transfer-target`) is
///      its service's and relays nowhere;
///   4. a caller request on a dialog that fronts nothing: the first confirmed
///      b-leg, else the first b-leg, on the dialog its requests ride;
///   5. a callee request: the a-leg.
///
/// Toward the a-leg the dialog is the live caller-facing one fronting the
/// source's dialog when the caller was shown it, else the caller's confirmed
/// dialog; toward a b-leg with no fork named, the dialog the leg's requests
/// ride ([`crate::Dialogs::carrying`]).
pub fn resolve_relay_peer(
    call: &Call,
    source_leg_id: &str,
    source: Option<&DialogId>,
) -> RelayPeer {
    let a_leg_id = &call.a_leg.leg_id;
    let toward = |leg_id: &str| -> RelayPeer {
        let dialog = if leg_id == a_leg_id {
            toward_caller(call, source_leg_id, source)
        } else {
            find_b_leg(call, leg_id).and_then(|l| l.dialogs.carrying()).map(Dialog::id)
        };
        RelayPeer::Leg { leg_id: leg_id.to_string(), dialog }
    };
    if let Some(peer) = get_peer(call, source_leg_id) {
        return toward(peer);
    }
    if source_leg_id == a_leg_id {
        if let Some(face) =
            source.and_then(|id| call.a_leg.dialogs.by_tags(&id.our_tag, &id.peer_tag))
        {
            if let Some((leg_id, fork)) = fronted_fork(call, face) {
                return RelayPeer::Leg { leg_id, dialog: fork.map(Dialog::id) };
            }
        }
        let peer = call
            .b_legs
            .iter()
            .find(|l| l.state == LegState::Confirmed)
            .or_else(|| call.b_legs.first());
        return match peer {
            Some(l) => toward(&l.leg_id),
            None => RelayPeer::NoLeg,
        };
    }
    if find_b_leg(call, source_leg_id).is_some_and(|leg| !is_adopted(leg)) {
        return RelayPeer::Unadopted;
    }
    toward(a_leg_id)
}

/// The caller-facing dialog a request from `source_leg_id`'s dialog `source`
/// (its peer tag the callee's) is relayed on: the live one fronting it
/// when the caller was shown it, else the caller's confirmed dialog. `None`
/// while the caller holds no dialog that callee dialog was shown under.
fn toward_caller(call: &Call, source_leg_id: &str, source: Option<&DialogId>) -> Option<DialogId> {
    let face = source
        .and_then(|id| fronting_dialog(call, source_leg_id, &id.peer_tag))
        .filter(|d| d.is_live());
    match face {
        Some(d) => (d.shown || d.was_established()).then(|| d.id()),
        None => call.a_leg.dialogs.confirmed().map(Dialog::id),
    }
}

/// The callee leg and dialog the caller-facing dialog `face` relays: the
/// first it fronts whose dialog carries requests, else the first it fronts.
/// `None` when it fronts none.
fn fronted_fork<'a>(call: &'a Call, face: &Dialog) -> Option<(String, Option<&'a Dialog>)> {
    let fork = |f: &crate::model::PeerDialog| {
        find_b_leg(call, &f.leg_id).and_then(|l| callee_dialog(l, &f.peer_tag))
    };
    let first = face.fronts.first()?;
    face.fronts
        .iter()
        .find_map(|f| fork(f).filter(|d| d.carries_requests()).map(|d| (f.leg_id.clone(), Some(d))))
        .or_else(|| Some((first.leg_id.clone(), fork(first))))
}

/// Is the relay target of an in-dialog request from `source_leg_id` in a
/// **relayable** state? `false` exactly when a `RelayToPeer` would go nowhere
/// useful: no peer leg or dialog resolves, or the peer leg is `Terminated`
/// (a failed b-leg whose `/call/failure` reroute is still pending). An
/// `Early` dialog is relayable — an early-dialog UPDATE is the RFC 3311 §5.1
/// normal case.
pub fn relay_peer_dialog_ready(
    call: &Call,
    source_leg_id: &str,
    source: Option<&DialogId>,
) -> bool {
    relay_peer_dialog(call, source_leg_id, source)
        .is_some_and(|(leg, _)| leg.state != LegState::Terminated)
}

/// The `(leg, dialog)` a relayed in-dialog request from `source_leg_id` would
/// be regenerated on, by [`resolve_relay_peer`]. The single resolver behind
/// both [`relay_peer_dialog_ready`] and the rule-vocabulary peer-dialog reads,
/// so match and action never disagree.
pub fn relay_peer_dialog<'a>(
    call: &'a Call,
    source_leg_id: &str,
    source: Option<&DialogId>,
) -> Option<(&'a Leg, &'a Dialog)> {
    let RelayPeer::Leg { leg_id, dialog: Some(id) } =
        resolve_relay_peer(call, source_leg_id, source)
    else {
        return None;
    };
    let leg = if leg_id == call.a_leg.leg_id { &call.a_leg } else { find_b_leg(call, &leg_id)? };
    Some((leg, leg.dialogs.by_tags(&id.our_tag, &id.peer_tag)?))
}
