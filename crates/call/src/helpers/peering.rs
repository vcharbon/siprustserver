//! Which leg talks to which: the tag-map dialog-identity index, the active
//! peer pair (INAP-style split/merge), and transparent-relay peer resolution.

use crate::model::{ActivePeer, Call, Dialog, Leg, LegState, TagMapping};

use super::leg::{find_b_leg, find_dialog_by_to_tag, is_adopted};

// ── Tag mapping ─────────────────────────────────────────────────────────────

/// Add a tag mapping keyed by `(bLegId, bTag)`. A duplicate key leaves the call
/// unchanged (the tagMap is a dialog-identity index).
pub fn add_tag_mapping(mut call: Call, mapping: TagMapping) -> Call {
    let exists = call
        .tag_map
        .iter()
        .any(|m| m.b_leg_id == mapping.b_leg_id && m.b_tag == mapping.b_tag);
    if !exists {
        call.tag_map.push(mapping);
    }
    call
}

/// Look up a mapping by the B2BUA's a-facing tag.
pub fn find_by_a_tag<'a>(call: &'a Call, a_tag: &str) -> Option<&'a TagMapping> {
    call.tag_map.iter().find(|m| m.a_tag == a_tag)
}

/// Look up a mapping by the B-leg's real tag.
pub fn find_by_b_tag<'a>(call: &'a Call, b_leg_id: &str, b_tag: &str) -> Option<&'a TagMapping> {
    call.tag_map
        .iter()
        .find(|m| m.b_leg_id == b_leg_id && m.b_tag == b_tag)
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
    call.active_peer = Some(ActivePeer {
        leg_a: leg_a.into(),
        leg_b: leg_b.into(),
    });
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
/// plus (for forking) the specific callee early-dialog tag. The ONE resolver
/// both the executor's relay path and the rule-vocabulary readiness predicate
/// ([`relay_peer_dialog_ready`]) share — keep them in lockstep by construction.
///
/// Order:
///   1. the active pair (post-merge) — after a failover the tag map still
///      carries a stale (same a-tag → failed b-leg) mapping, so the merge's
///      `active_peer` (the live leg) must win;
///   2. pre-merge a-leg in-dialog request (forking PRACK/UPDATE): the request's
///      To-tag (the B2BUA's a-facing tag) resolves via the tag map to the right
///      b-leg + callee fork tag;
///   3. implicit b→a fallback, gated on leg adoption (ADR-0014): an unadopted
///      leg (parked `media`, un-realigned `transfer-target`) is owned by its
///      service rule and must never be mis-routed to A;
///   4. fallback pairing: a-leg ↔ (first confirmed b-leg, else first b-leg).
pub fn resolve_relay_peer(
    call: &Call,
    source_leg_id: &str,
    request_to_tag: Option<&str>,
) -> (Option<String>, Option<String>) {
    if let Some(p) = &call.active_peer {
        if p.leg_a == source_leg_id {
            return (Some(p.leg_b.clone()), None);
        }
        if p.leg_b == source_leg_id {
            return (Some(p.leg_a.clone()), None);
        }
    }
    if source_leg_id == call.a_leg.leg_id {
        if let Some(tag) = request_to_tag {
            if let Some(m) = find_by_a_tag(call, tag) {
                return (Some(m.b_leg_id.clone()), Some(m.b_tag.clone()));
            }
        }
    } else if let Some(leg) = find_b_leg(call, source_leg_id) {
        if !is_adopted(leg) {
            return (None, None);
        }
    }
    if source_leg_id == call.a_leg.leg_id {
        let peer = call
            .b_legs
            .iter()
            .find(|l| l.state == LegState::Confirmed)
            .or_else(|| call.b_legs.first())
            .map(|l| l.leg_id.clone());
        return (peer, None);
    }
    (Some(call.a_leg.leg_id.clone()), None)
}

/// Is the relay target of an in-dialog request from `source_leg_id` in a
/// **relayable** state? `false` exactly when a `RelayToPeer` would go nowhere
/// useful: no peer leg resolves, the peer leg is `Terminated` (a failed b-leg
/// whose `/call/failure` reroute is still pending), or the target dialog
/// carries no remote tag yet (a replacement leg still `Trying` — the relay
/// machinery cannot mint a well-formed in-dialog request, so the relay path
/// would silently drop it). An `Early` dialog WITH a remote tag is relayable —
/// an early-dialog UPDATE is the RFC 3311 §5.1 normal case.
pub fn relay_peer_dialog_ready(
    call: &Call,
    source_leg_id: &str,
    request_to_tag: Option<&str>,
) -> bool {
    relay_peer_dialog(call, source_leg_id, request_to_tag)
        .is_some_and(|(leg, d)| leg.state != LegState::Terminated && !d.sip.remote_tag.is_empty())
}

/// The `(leg, dialog)` a relayed in-dialog request from `source_leg_id` would
/// be regenerated on: [`resolve_relay_peer`]'s leg pick, then the relay path's
/// dialog pick (the fork tag's dialog, else the first). The single resolver
/// behind both [`relay_peer_dialog_ready`] and the rule-vocabulary peer-dialog
/// reads, so match and action never disagree.
pub fn relay_peer_dialog<'a>(
    call: &'a Call,
    source_leg_id: &str,
    request_to_tag: Option<&str>,
) -> Option<(&'a Leg, &'a Dialog)> {
    let (peer, fork_tag) = resolve_relay_peer(call, source_leg_id, request_to_tag);
    let peer_id = peer?;
    let leg = if peer_id == call.a_leg.leg_id {
        &call.a_leg
    } else {
        find_b_leg(call, &peer_id)?
    };
    let dialog = fork_tag
        .as_deref()
        .and_then(|tt| find_dialog_by_to_tag(leg, tt))
        .or_else(|| leg.dialogs.first())?;
    Some((leg, dialog))
}
