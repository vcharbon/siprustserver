//! A callee leg's INVITE provisional that this stack does not show the
//! originator: when that is so, and what the ringing leg is still owed.

use call::CdrEventType;
use sip_message::header;
use sip_message::SipResponse;

use crate::model::{RuleAction, RuleCall, RuleContext};

/// Whether the originator's initial INVITE server transaction sent its final
/// (RFC 3261 §17.2.1). A completed INVITE transaction emits no further
/// provisional (§13.3.1.1), so a leg ringing after this point — one dialled
/// mid-call, or one still ringing after her CANCEL — rings inside a dialog
/// she holds and is shown to her as nothing.
pub fn originator_final_sent(call: &RuleCall) -> bool {
    call.a_leg().invite_final_sent().is_some()
}

/// The `RSeq` a reliable provisional states (RFC 3262: `Require: 100rel` plus
/// a numeric `RSeq`), or `None` when this response is not one.
pub fn reliable_rseq(resp: &SipResponse) -> Option<i64> {
    let requires = resp.header::<header::Require>()?.ok()?;
    if !requires.contains("100rel") {
        return None;
    }
    Some(resp.header::<header::RSeq>()?.ok()?.value() as i64)
}

/// Whether the event's INVITE provisional is shown to no one: the
/// originator's INVITE transaction sent its final, or the leg it came from
/// has no relay peer (a service's unadopted leg).
pub fn shown_to_no_one(ctx: &RuleContext) -> bool {
    originator_final_sent(&ctx.call) || !ctx.relay_peer_resolves()
}

/// What the event's INVITE provisional is owed on the leg it came from when
/// no one is shown it: the early dialog it establishes (RFC 3261 §12.1.2), so
/// the leg reads `Early` and a later teardown CANCELs it, and the CDR carrying
/// it. A reliable provisional's early dialog and PRACK are the engine's,
/// which recorded it before any rule ran (RFC 3262 §4). Empty for an event
/// that is not a response.
pub fn absorbed_provisional_actions(ctx: &RuleContext) -> Vec<RuleAction> {
    let Some(resp) = ctx.response() else { return Vec::new() };
    let leg = ctx.source_leg_id.to_string();
    let b_tag = resp.to().tag().unwrap_or_default().to_string();
    let mut actions = Vec::new();
    if !b_tag.is_empty() && reliable_rseq(resp).is_none() {
        actions.push(RuleAction::TrackEarlyDialog { leg_id: leg.clone(), b_tag });
    }
    actions.push(RuleAction::AddCdrEvent {
        event_type: CdrEventType::Provisional,
        leg_id: leg,
        status_code: Some(i64::from(resp.status())),
        reason: None,
    });
    actions
}
