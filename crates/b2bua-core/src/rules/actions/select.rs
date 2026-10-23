//! Leg / dialog selection views over [`Call`] shared by the action-family
//! modules: pending-relay lookup, peer resolution and index-based leg access.
//! Pure reads — no state mutation, no SIP building.

use call::{Call, Dialog, DialogId};
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};

use b2bua_sdk::model::RuleContext;

use call::helpers::{find_leg, find_relay};

/// The dialog of `leg_id` (the a-leg or a b-leg) holding the relayed
/// re-INVITE snapshot for `outbound_cseq`: the leg's established dialog, the
/// only one a re-INVITE rides (RFC 3261 §14.1); its identity plus an owned
/// clone.
pub(super) fn find_pending_dialog(
    call: &Call,
    leg_id: &str,
    outbound_cseq: i64,
) -> Option<(DialogId, Dialog)> {
    let d = find_leg(call, leg_id)?.dialogs.established()?;
    find_relay(d, outbound_cseq).map(|_| (d.id(), d.clone()))
}

/// The INVITE CSeq cached on a dialog's pending INVITE handle (RFC 3261
/// §13.2.2.4 / RFC 3262 §7.2). Parses the snapshot; `None` if absent/unparseable.
pub(super) fn invite_cseq_from_handle(dialog: &Dialog) -> Option<i64> {
    let handle = dialog.pending_invite_txn.as_ref()?;
    match CustomParser::new().parse(&handle.original_invite).ok()? {
        SipMessage::Request(r) => Some(r.cseq().seq() as i64),
        _ => None,
    }
}

/// Resolve the relay target leg and the dialog of it the relay rides. Thin
/// view over [`call::helpers::resolve_relay_peer`] — the ONE resolver this
/// relay path shares with the rule-vocabulary readiness predicate
/// (`RuleContext::peer_relay_ready`), so the two never drift.
pub(super) fn resolve_peer(call: &Call, ctx: &RuleContext) -> call::helpers::RelayPeer {
    call::helpers::resolve_relay_peer(call, ctx.source_leg_id, ctx.request_dialog_id())
}

/// The leg `leg_id` names on `call`: the a-leg or one of its b-legs.
pub(super) fn leg_at<'a>(call: &'a Call, leg_id: &str) -> Option<&'a call::Leg> {
    call::helpers::find_leg(call, leg_id)
}

/// Project a canonical [`Method`] onto the in-dialog admissibility view the
/// in-dialog generators accept — `None` for methods that may not be sent as an
/// ordinary in-dialog request (ACK/CANCEL, out-of-dialog-only).
pub(super) fn in_dialog_method(method: &Method) -> Option<InDialogMethod> {
    InDialogMethod::try_from(method).ok()
}
