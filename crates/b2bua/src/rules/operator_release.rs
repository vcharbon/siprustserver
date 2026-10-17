//! `operator-release` (ADR-0050 §10): the turn of one call an operator's
//! release-all reached ([`crate::release_all`]). An answered call ends with
//! nothing sent: every leg is let go as it stands, so no BYE leaves, and the
//! ordinary release closes its record, frees its holds and deletes its
//! replica. A call in setup is aborted: `503` to the caller, then the ordinary
//! teardown CANCELs a ringing callee. A call already ending is left to its
//! teardown: the rule does not run while terminating.

use call::{CdrEventType, TerminationCause};

use b2bua_sdk::model::{
    Match, RuleAction, RuleContext, RuleDefinition, RuleHandleResult, CORE_LAYER,
};

use crate::release_all::{REASON, TOPIC};
use crate::rules::defaults::force_terminal;

/// The operator-release rule family: CORE, its match unique to its topic.
pub fn operator_release_rules() -> Vec<RuleDefinition> {
    vec![RuleDefinition::core(
        "operator-release",
        CORE_LAYER,
        &[],
        Match::internal_event().topic(TOPIC),
        operator_release,
    )]
}

fn operator_release(ctx: &RuleContext) -> Option<RuleHandleResult> {
    let cause = TerminationCause::OperatorRelease;
    if ctx.call.caller_answered() {
        return force_terminal(ctx, REASON, cause);
    }
    Some(RuleHandleResult::new(vec![
        RuleAction::AddCdrEvent {
            event_type: CdrEventType::Bye,
            leg_id: ctx.call.a_leg().leg_id.clone(),
            status_code: None,
            reason: Some(REASON.into()),
        },
        RuleAction::RespondToALeg {
            status: 503,
            reason: "Service Unavailable".into(),
            header_updates: vec![],
            contacts: vec![],
        },
        RuleAction::BeginTermination { reason: Some(REASON.into()), cause, by_leg: None },
    ]))
}
