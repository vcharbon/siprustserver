//! Leg / dialog selection views over [`Call`] shared by the action-family
//! modules: identity-tag derivation, pending-relay lookup, peer resolution and
//! index-based leg access. Pure reads — no state mutation, no SIP building.

use call::{Call, Dialog};
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};

use crate::rules::model::RuleContext;

use call::helpers::find_pending_request;

/// Dialog identity tag: a-leg → its local (B2BUA) tag; b-leg → the remote
/// (callee) tag. Selects the dialog within a leg.
pub(super) fn dialog_identity_tag(leg_id: &str, dialog: &Dialog) -> String {
    if leg_id == "a" {
        dialog.sip.local_tag.clone()
    } else {
        dialog.sip.remote_tag.clone()
    }
}

/// Find the dialog on `leg_id` (the a-leg or a b-leg) holding the pending
/// transparent-relay snapshot for `outbound_cseq`; returns its identity tag
/// (dialog selector for the lens helpers) plus an owned clone of the dialog.
pub(super) fn find_pending_dialog(
    call: &Call,
    leg_id: &str,
    outbound_cseq: i64,
) -> Option<(String, Dialog)> {
    let leg = if call.a_leg.leg_id == leg_id {
        &call.a_leg
    } else {
        call.b_legs.iter().find(|l| l.leg_id == leg_id)?
    };
    leg.dialogs
        .iter()
        .find(|d| find_pending_request(d, outbound_cseq).is_some())
        .map(|d| (dialog_identity_tag(leg_id, d), d.clone()))
}

/// The INVITE CSeq cached on a dialog's pending INVITE handle (RFC 3261
/// §13.2.2.4 / RFC 3262 §7.2). Parses the snapshot; `None` if absent/unparseable.
pub(super) fn invite_cseq_from_handle(dialog: &Dialog) -> Option<i64> {
    let handle = dialog.ext.pending_invite_txn.as_ref()?;
    match CustomParser::new().parse(&handle.original_invite).ok()? {
        SipMessage::Request(r) => Some(r.cseq().seq() as i64),
        _ => None,
    }
}

/// Resolve the relay target leg plus, for forking, the specific callee early-
/// dialog tag. Thin view over [`call::helpers::resolve_relay_peer`] — the ONE
/// resolver this relay path shares with the rule-vocabulary readiness predicate
/// (`RuleContext::peer_relay_ready`), so the two never drift.
pub(super) fn resolve_peer(call: &Call, ctx: &RuleContext) -> (Option<String>, Option<String>) {
    let to_tag = ctx.request().and_then(|r| r.to().tag());
    call::helpers::resolve_relay_peer(call, ctx.source_leg_id, to_tag)
}

pub(super) fn leg_index(call: &Call, leg_id: &str) -> Option<usize> {
    if leg_id == call.a_leg.leg_id {
        Some(usize::MAX)
    } else {
        call.b_legs.iter().position(|l| l.leg_id == leg_id)
    }
}

pub(super) fn leg_at(call: &Call, idx: usize) -> &call::Leg {
    if idx == usize::MAX {
        &call.a_leg
    } else {
        &call.b_legs[idx]
    }
}

/// Project a canonical [`Method`] onto the in-dialog admissibility view the
/// in-dialog generators accept — `None` for methods that may not be sent as an
/// ordinary in-dialog request (ACK/CANCEL, out-of-dialog-only).
pub(super) fn in_dialog_method(method: &Method) -> Option<InDialogMethod> {
    InDialogMethod::try_from(method).ok()
}
