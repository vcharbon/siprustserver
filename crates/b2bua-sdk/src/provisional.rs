//! A callee leg's INVITE provisional that this stack does not show the
//! originator: when that is so, and what the ringing leg is still owed.

use call::{CdrEventType, LegState};
use sip_message::header;
use sip_message::SipResponse;

use crate::model::{RuleAction, RuleCall, RuleContext};

/// Whether the originator's initial INVITE server transaction sent its final
/// (RFC 3261 §17.2.1). A completed INVITE transaction emits no further
/// provisional (§13.3.1.1), so a leg ringing after this point — one dialled
/// mid-call, or one still ringing after her CANCEL — rings inside a dialog
/// she holds and is shown to her as nothing.
pub fn originator_final_sent(call: &RuleCall) -> bool {
    call.a_leg().invite_final_sent.is_some()
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

/// What the event's INVITE provisional is owed on the leg it came from when
/// no one is shown it: the early dialog it establishes (RFC 3261 §12.1.2),
/// the leg `Early` (a later teardown CANCELs it), its PRACK
/// ([`owed_prack`]), and the CDR carrying it. A reliable provisional already
/// acknowledged is the responder's retransmission and is owed nothing (§4).
/// Empty for an event that is not a response.
pub fn absorbed_provisional_actions(ctx: &RuleContext) -> Vec<RuleAction> {
    let Some(resp) = ctx.response() else { return Vec::new() };
    let leg = ctx.source_leg_id.to_string();
    let b_tag = resp.to().tag().unwrap_or_default().to_string();
    let prack = owed_prack(ctx);
    if reliable_rseq(resp).is_some() && prack.is_none() {
        return Vec::new();
    }
    let mut actions = Vec::new();
    if !b_tag.is_empty() {
        actions.push(RuleAction::TrackEarlyDialog { leg_id: leg.clone(), b_tag });
    }
    actions.push(RuleAction::UpdateLegState {
        leg_id: leg.clone(),
        state: LegState::Early,
        disposition: None,
    });
    actions.extend(prack);
    actions.push(RuleAction::AddCdrEvent {
        event_type: CdrEventType::Provisional,
        leg_id: leg,
        status_code: Some(i64::from(resp.status())),
        reason: None,
    });
    actions
}

/// The PRACK this stack owes the event's reliable provisional when it shows
/// it to no one: this stack is the leg's UAC and the only party that took it,
/// so it acknowledges it on the provisional's own early dialog (RFC 3262 §4),
/// a CANCEL already sent notwithstanding. `None` for a response that is not a
/// reliable provisional, and for one already acknowledged — by this stack or
/// by the party it was relayed to — whose repeat is the responder's
/// retransmission.
pub fn owed_prack(ctx: &RuleContext) -> Option<RuleAction> {
    let resp = ctx.response()?;
    let rseq = reliable_rseq(resp)?;
    let leg = ctx.source_leg_id;
    let b_tag = resp.to().tag().unwrap_or_default();
    let invite_cseq = i64::from(resp.cseq().seq());
    if ctx.call.acknowledged_provisional(leg, b_tag, invite_cseq, rseq) {
        return None;
    }
    Some(RuleAction::SendPrackToLeg {
        leg_id: leg.to_string(),
        rseq,
        invite_cseq,
        b_tag: b_tag.to_string(),
    })
}
