//! Pure lens / accessor / timer helpers over the [`Call`] tree — port of the
//! helper half of `src/call/CallModel.ts` plus `src/call/timer-helpers.ts`.
//!
//! The source helpers are immutable (object spread returns a new value). Here
//! they take the `Call`/`Leg` by value, mutate in place, and return it — same
//! value semantics from the caller's view, without deep clones.
//!
//! The RNG seam is deferred (ADR-0008): the dialog constructors take the initial
//! CSeq as a parameter instead of drawing it from a fiber-local `Random`. When
//! the stateful CallState slice lands, the CSeq is drawn from `sip-txn`'s `IdGen`.

use std::collections::BTreeMap;

use crate::model::{
    ActivePeer, B2buaDialogExt, ByeDisposition, Call, CdrEvent, Dialog, ExtMap, Leg,
    LegDisposition, LegKind, LegState, PendingRequest, StackDialog, TagMapping, TimerEntry,
};

/// Safety-net timer delay for the `terminating` state (ms). 32 s = SIP Timer
/// H/J — beyond it no legitimate BYE/2xx retransmit can land. See
/// `timer-helpers.ts` for the full rationale.
pub const TERMINATING_TIMEOUT_MS: i64 = 32_000;

// ── Leg role ────────────────────────────────────────────────────────────────

/// Resolve a leg's role, defaulting from `legId` when `kind` is absent.
pub fn leg_kind(leg: &Leg) -> LegKind {
    leg.kind.unwrap_or(if leg.leg_id == "a" {
        LegKind::A
    } else {
        LegKind::Destination
    })
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

/// Dialog-identity match — a-leg keys off `localTag`, b-leg off `remoteTag`.
fn match_dialog_identity(leg_id: &str, identity_tag: &str, d: &Dialog) -> bool {
    if leg_id == "a" {
        d.sip.local_tag == identity_tag
    } else {
        d.sip.remote_tag == identity_tag
    }
}

// ── Lens helpers ─────────────────────────────────────────────────────────────

/// Apply `f` to the leg with `leg_id` (a-leg or any matching b-leg).
pub fn update_leg(mut call: Call, leg_id: &str, f: impl FnOnce(&mut Leg)) -> Call {
    if call.a_leg.leg_id == leg_id {
        f(&mut call.a_leg);
    } else if let Some(l) = call.b_legs.iter_mut().find(|l| l.leg_id == leg_id) {
        f(l);
    }
    call
}

/// Apply `f` to every dialog within `leg_id` matching `identity_tag`.
pub fn update_dialog(
    call: Call,
    leg_id: &str,
    identity_tag: &str,
    mut f: impl FnMut(&mut Dialog),
) -> Call {
    update_leg(call, leg_id, |leg| {
        for d in &mut leg.dialogs {
            if match_dialog_identity(leg_id, identity_tag, d) {
                f(d);
            }
        }
    })
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

/// Whether all legs of a terminating call have reached a terminal BYE
/// disposition. A `trying` leg with no `byeDisposition` never established, so it
/// is considered resolved.
pub fn is_fully_resolved(call: &Call) -> bool {
    std::iter::once(&call.a_leg)
        .chain(call.b_legs.iter())
        .all(|leg| match leg.bye_disposition {
            None => leg.state == LegState::Trying,
            Some(b) => b.is_terminal(),
        })
}

/// Append a CDR event.
pub fn add_cdr_event(mut call: Call, event: CdrEvent) -> Call {
    call.cdr_events.push(event);
    call
}

/// Add a new b-leg.
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

// ── CSeq helpers ─────────────────────────────────────────────────────────────

/// Bump a dialog's local CSeq by `delta` (CSeq is dialog-scoped — §12.2.1.1).
/// Each dialog (including each forked early dialog, keyed by its own remote tag)
/// owns an independent sequence, so a sibling fork's CSeq never constrains this one.
pub fn bump_local_cseq(call: Call, leg_id: &str, identity_tag: &str, delta: i64) -> Call {
    update_dialog(call, leg_id, identity_tag, |d| d.sip.local_cseq += delta)
}

/// Track the other side's latest CSeq on a dialog.
pub fn update_remote_cseq(call: Call, leg_id: &str, identity_tag: &str, remote_cseq: i64) -> Call {
    update_dialog(call, leg_id, identity_tag, |d| {
        d.ext.remote_cseq = Some(remote_cseq)
    })
}

/// CSeq delta for a relayed request: `inbound - sourceRemoteCSeq`, clamped ≥ 1.
pub fn relay_cseq_delta(inbound_cseq: i64, source_remote_cseq: Option<i64>) -> i64 {
    match source_remote_cseq {
        None => 1,
        Some(s) => (inbound_cseq - s).max(1),
    }
}

// ── Pending transparent-relay request helpers ───────────────────────────────

/// Add a pending transparent-relay entry to a dialog.
pub fn add_pending_request(
    call: Call,
    leg_id: &str,
    identity_tag: &str,
    entry: PendingRequest,
) -> Call {
    update_dialog(call, leg_id, identity_tag, |d| {
        d.ext.inbound_pending_requests.push(entry.clone())
    })
}

/// Find a pending transparent-relay entry by outbound CSeq.
pub fn find_pending_request(dialog: &Dialog, outbound_cseq: i64) -> Option<&PendingRequest> {
    dialog
        .ext
        .inbound_pending_requests
        .iter()
        .find(|p| p.outbound_cseq == outbound_cseq)
}

/// Remove a pending transparent-relay entry after its response is handled.
pub fn remove_pending_request(
    call: Call,
    leg_id: &str,
    identity_tag: &str,
    outbound_cseq: i64,
) -> Call {
    update_dialog(call, leg_id, identity_tag, |d| {
        d.ext
            .inbound_pending_requests
            .retain(|p| p.outbound_cseq != outbound_cseq)
    })
}

// ── Leg tag helpers ─────────────────────────────────────────────────────────

/// The B2BUA's own tag for a leg (`sip.localTag` of `dialogs[0]`).
pub fn b2bua_tag(call: &Call, leg_id: &str) -> Option<String> {
    if leg_id == "a" {
        return call.a_leg.dialogs.first().map(|d| d.sip.local_tag.clone());
    }
    let b = find_b_leg(call, leg_id)?;
    Some(
        b.dialogs
            .first()
            .map(|d| d.sip.local_tag.clone())
            .unwrap_or_else(|| b.from_tag.clone()),
    )
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

// ── Tag mapping helpers ──────────────────────────────────────────────────────

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

// ── Active peer helpers (INAP-style split/merge) ────────────────────────────

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

// ── relayFirst18xTo180 runtime-state helpers ────────────────────────────────

/// The active `relayFirst18xTo180` strategy for this call, if any.
pub fn relay_first_18x_strategy(call: &Call) -> Option<crate::features::RelayFirst18xStrategy> {
    call.features
        .as_ref()
        .and_then(|f| f.relay_first_18x_to_180.as_ref())
        .map(|r| r.strategy)
}

/// Whether the first 18x has already been relayed under the strategy.
pub fn relay_first_18x_first_relayed(call: &Call) -> bool {
    call.relay_first_18x
        .as_ref()
        .map(|s| s.first_relayed)
        .unwrap_or(false)
}

/// The a-facing To-tag minted on the first 18x (reused on the 200 OK).
pub fn relay_first_18x_stored_a_tag(call: &Call) -> Option<&str> {
    call.relay_first_18x
        .as_ref()
        .and_then(|s| s.stored_a_tag.as_deref())
}

/// Mark the first 18x relayed and record the minted a-facing tag.
pub fn set_relay_first_18x_relayed(mut call: Call, stored_a_tag: &str) -> Call {
    call.relay_first_18x = Some(crate::model::RelayFirst18xState {
        first_relayed: true,
        stored_a_tag: Some(stored_a_tag.to_string()),
    });
    call
}

/// Cache an SDP body on a b-leg dialog selected by its callee (remote) tag,
/// falling back to the leg's first dialog when no tag matches.
pub fn cache_sdp_on_leg_dialog(mut call: Call, leg_id: &str, b_tag: &str, body: Vec<u8>) -> Call {
    if let Some(leg) = call.b_legs.iter_mut().find(|l| l.leg_id == leg_id) {
        let idx = leg
            .dialogs
            .iter()
            .position(|d| d.sip.remote_tag == b_tag)
            .or(if leg.dialogs.is_empty() { None } else { Some(0) });
        if let Some(i) = idx {
            leg.dialogs[i].ext.cached_sdp = Some(body);
        }
    }
    call
}

/// The SDP cached on a b-leg dialog selected by callee tag (else first dialog).
pub fn cached_sdp_for_leg_dialog<'a>(call: &'a Call, leg_id: &str, b_tag: &str) -> Option<&'a [u8]> {
    let leg = call.b_legs.iter().find(|l| l.leg_id == leg_id)?;
    let dialog = leg
        .dialogs
        .iter()
        .find(|d| d.sip.remote_tag == b_tag)
        .or_else(|| leg.dialogs.first())?;
    dialog.ext.cached_sdp.as_deref()
}

// ── promote18xPemTo200 runtime-state helpers ────────────────────────────────

/// The current PEM runtime slice, if any.
pub fn promote_pem_state(call: &Call) -> Option<&crate::model::PromotePemState> {
    call.promote_pem.as_ref()
}

/// Whether the first 183+PEM has been promoted to a synthetic 200 OK.
pub fn promote_pem_promoted(call: &Call) -> bool {
    call.promote_pem.as_ref().map(|s| s.promoted).unwrap_or(false)
}

/// Whether the promotion window is open (Alice's in-dialog requests rejected).
pub fn promote_pem_window_open(call: &Call) -> bool {
    call.promote_pem.as_ref().map(|s| s.window_open).unwrap_or(false)
}

/// Overwrite the PEM runtime slice (`None` resets to the pre-promotion state).
pub fn set_promote_pem(mut call: Call, state: Option<crate::model::PromotePemState>) -> Call {
    call.promote_pem = state;
    call
}

// ── REFER transfer service slice (port of TS `TransferCallExt` accessors) ────

/// The current REFER transfer runtime slice, if any.
pub fn transfer_state(call: &Call) -> Option<&crate::model::TransferState> {
    call.transfer.as_ref()
}

/// The current transfer phase, if a transfer is active.
pub fn transfer_phase(call: &Call) -> Option<crate::model::TransferPhase> {
    call.transfer.as_ref().map(|s| s.phase)
}

/// Whether a transfer is active (the slice is present) — the service guard
/// (mirrors the TS `noTransferActive` negation).
pub fn transfer_active(call: &Call) -> bool {
    call.transfer.is_some()
}

/// Overwrite the transfer runtime slice (`None` clears it — the terminal path).
pub fn set_transfer(mut call: Call, state: Option<crate::model::TransferState>) -> Call {
    call.transfer = state;
    call
}

// ── Service ext + rule helpers (ADR-0016) ───────────────────────────────────

/// Write an encoded ext slice into `call.ext[serviceId]`; `None` drops the key.
pub fn set_call_ext(mut call: Call, service_id: &str, value: Option<serde_json::Value>) -> Call {
    match value {
        None => {
            if let Some(ext) = &mut call.ext {
                ext.remove(service_id);
            }
        }
        Some(v) => {
            call.ext
                .get_or_insert_with(ExtMap::new)
                .insert(service_id.to_string(), v);
        }
    }
    call
}

/// Write an encoded ext slice into the named leg's `ext[serviceId]`.
pub fn set_leg_ext(
    call: Call,
    leg_id: &str,
    service_id: &str,
    value: serde_json::Value,
) -> Call {
    update_leg(call, leg_id, |leg| {
        leg.ext
            .get_or_insert_with(ExtMap::new)
            .insert(service_id.to_string(), value);
    })
}

/// Deactivate a rule (set `active = false`); preserves the entry for tracing.
pub fn deactivate_rule(mut call: Call, rule_id: &str) -> Call {
    let mut rules = call.active_rules.take().unwrap_or_default();
    for r in &mut rules {
        if r.id == rule_id {
            r.active = false;
        }
    }
    call.active_rules = Some(rules);
    call
}

/// CSeq number from the retained a-leg INVITE (`1` if no parseable CSeq header).
pub fn a_leg_invite_cseq_num(call: &Call) -> i64 {
    for h in &call.a_leg_invite.headers {
        if h.name.eq_ignore_ascii_case("cseq") {
            if let Some(n) = h.value.split_whitespace().next().and_then(|s| s.parse().ok()) {
                return n;
            }
        }
    }
    1
}

// ── Dialog constructors ──────────────────────────────────────────────────────

/// Fields a dialog constructor needs from the enclosing leg.
pub struct MakeDialogLegCtx<'a> {
    pub call_id: &'a str,
    pub local_uri: &'a str,
    pub remote_uri: &'a str,
    pub local_tag: &'a str,
    pub remote_tag: &'a str,
}

fn stack_dialog(ctx: &MakeDialogLegCtx, initial_cseq: i64, route_set: Vec<String>) -> StackDialog {
    StackDialog {
        call_id: ctx.call_id.to_string(),
        local_tag: ctx.local_tag.to_string(),
        remote_tag: ctx.remote_tag.to_string(),
        local_uri: ctx.local_uri.to_string(),
        remote_uri: ctx.remote_uri.to_string(),
        remote_target: String::new(),
        local_cseq: initial_cseq,
        route_set,
    }
}

/// Build the initial empty dialog stub. `initial_cseq` is supplied by the caller
/// (the RNG seam is deferred — see module docs).
pub fn make_empty_dialog(ctx: &MakeDialogLegCtx, initial_cseq: i64) -> Dialog {
    Dialog {
        sip: stack_dialog(ctx, initial_cseq, Vec::new()),
        ext: B2buaDialogExt {
            remote_cseq: None,
            inbound_pending_requests: Vec::new(),
            ack_branch: None,
            pending_invite_txn: None,
            cached_sdp: None,
        },
    }
}

/// Build a dialog initialised from a received request's CSeq. `routeSet` carries
/// the dialog-creating request's `Record-Route` headers in order (§12.1.1).
pub fn make_dialog_from_incoming(
    ctx: &MakeDialogLegCtx,
    remote_cseq: i64,
    route_set: Vec<String>,
    initial_cseq: i64,
) -> Dialog {
    Dialog {
        sip: stack_dialog(ctx, initial_cseq, route_set),
        ext: B2buaDialogExt {
            remote_cseq: Some(remote_cseq),
            inbound_pending_requests: Vec::new(),
            ack_branch: None,
            pending_invite_txn: None,
            cached_sdp: None,
        },
    }
}

// ── Timer-list helper (port of timer-helpers.ts) ────────────────────────────

/// Replace any existing entry with the same id, then append the new one. Panics
/// in debug builds if two entries end up sharing an id (an upstream caller
/// bypassed this helper).
pub fn replace_timer_by_id(existing: Vec<TimerEntry>, entry: TimerEntry) -> Vec<TimerEntry> {
    let mut next: Vec<TimerEntry> = existing.into_iter().filter(|t| t.id != entry.id).collect();
    next.push(entry);
    debug_assert!(
        {
            let mut seen = BTreeMap::new();
            next.iter().all(|t| seen.insert(t.id.clone(), ()).is_none())
        },
        "replace_timer_by_id invariant violated: duplicate timer id"
    );
    next
}
