//! Dialog-level helpers: CSeq bookkeeping, the ACK-branch retention rule,
//! pending transparent-relay entries, the per-dialog SDP cache, and the dialog
//! constructors.

use crate::model::{B2buaDialogExt, Call, Dialog, PendingRequest, StackDialog};

use super::lens::{update_dialog, update_leg};

// ── CSeq ────────────────────────────────────────────────────────────────────

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

// ── ACK branch ──────────────────────────────────────────────────────────────

/// Retain the Via branch of the ACK-for-2xx just sent on `leg_id`'s dialog, so a
/// re-ACK of a **retransmitted** 2xx reuses the SAME branch (RFC 3261 §13.2.2.4).
/// A fresh branch would mint a new client transaction and never quiesce the
/// answerer's INVITE server txn, leaking / late-timing-out the confirmed call
/// when the first ACK is lost. Idempotent: only the first ACK writes it (every
/// re-ACK re-observes the same value). Reset to `None` wherever a new INVITE
/// transaction is cached on the dialog, so the stored branch always belongs to
/// the current INVITE's CSeq. Mirrors the `dialogs.first()` dialog choice in
/// `rules::relay::ack_b_leg`, its sole writer's twin.
pub fn retain_ack_branch(call: Call, leg_id: &str, branch: &str) -> Call {
    update_leg(call, leg_id, |leg| {
        if let Some(d) = leg.dialogs.first_mut() {
            if d.ext.ack_branch.is_none() {
                d.ext.ack_branch = Some(branch.to_string());
            }
        }
    })
}

// ── Pending transparent-relay requests ──────────────────────────────────────

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

/// Mark a pending transparent-relay entry CANCELled (RFC 3261 §9): the relayed
/// (re-)INVITE it snapshots has been CANCELled toward its target, so the
/// eventual final response resolves locally instead of being relayed back.
pub fn cancel_pending_request(
    call: Call,
    leg_id: &str,
    identity_tag: &str,
    outbound_cseq: i64,
) -> Call {
    update_dialog(call, leg_id, identity_tag, |d| {
        if let Some(p) = d
            .ext
            .inbound_pending_requests
            .iter_mut()
            .find(|p| p.outbound_cseq == outbound_cseq)
        {
            p.cancelled = true;
        }
    })
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

// ── Per-dialog SDP cache ────────────────────────────────────────────────────

/// Cache an SDP body on a b-leg dialog selected **strictly** by its callee
/// (remote) tag. No first-dialog fallback: under downstream forking each early
/// dialog carries its own answer (RFC 3264 §4), and a fallback write would
/// overwrite a *different* fork's cache. A miss is a no-op — the executor
/// ensures the `(leg, b_tag)` dialog exists before caching.
pub fn cache_sdp_on_leg_dialog(mut call: Call, leg_id: &str, b_tag: &str, body: Vec<u8>) -> Call {
    if let Some(leg) = call.b_legs.iter_mut().find(|l| l.leg_id == leg_id) {
        if let Some(d) = leg.dialogs.iter_mut().find(|d| d.sip.remote_tag == b_tag) {
            d.ext.cached_sdp = Some(body);
        }
    }
    call
}

/// The SDP cached on a b-leg dialog selected **strictly** by callee tag (no
/// first-dialog fallback — a different fork's cache must never leak into this
/// dialog's answer).
pub fn cached_sdp_for_leg_dialog<'a>(call: &'a Call, leg_id: &str, b_tag: &str) -> Option<&'a [u8]> {
    let leg = call.b_legs.iter().find(|l| l.leg_id == leg_id)?;
    let dialog = leg.dialogs.iter().find(|d| d.sip.remote_tag == b_tag)?;
    dialog.ext.cached_sdp.as_deref()
}

// ── Constructors ────────────────────────────────────────────────────────────

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

/// Build the initial empty dialog stub. `initial_cseq` is supplied by the
/// caller (the RNG seam is deferred — ADR-0008).
pub fn make_empty_dialog(ctx: &MakeDialogLegCtx, initial_cseq: i64) -> Dialog {
    Dialog {
        sip: stack_dialog(ctx, initial_cseq, Vec::new()),
        ext: B2buaDialogExt {
            remote_cseq: None,
            inbound_pending_requests: Vec::new(),
            ack_branch: None,
            pending_invite_txn: None,
            cached_sdp: None,
            pending_reinvite_2xx: None,
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
            pending_reinvite_2xx: None,
        },
    }
}
