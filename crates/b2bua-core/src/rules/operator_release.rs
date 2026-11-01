//! `operator-release` (ADR-0050 §10): the turn of one call an operator's
//! release-all reached, sent as a [`TOPIC`] event; a drain's grace sends the
//! same event, with the [`AT_DRAIN_GRACE`] outcome, to each call no peer holds
//! (ADR-0055 §3). A call already ending is left to its teardown: the rule does
//! not run while terminating.
//!
//! - An answered call ends with no BYE: a 2xx whose ACK this stack can compose
//!   is ACKed (RFC 3261 §13.2.2.4), a leg still ringing is CANCELled
//!   (§9.1), every other leg is let go as it stands. The ordinary release then
//!   closes the record, frees the holds and deletes every stored copy.
//! - A call in setup is aborted: `503` to the caller, then the ordinary
//!   teardown CANCELs a ringing callee.

use call::helpers::leg_is_resolved;
use call::{CdrEventType, TerminationCause};

use b2bua_sdk::model::{
    LegEnd, Match, RuleAction, RuleContext, RuleDefinition, RuleHandleResult, CORE_LAYER,
};

use b2bua_sdk::event::CallEvent;

use crate::rules::defaults::still_owed_bare_ack;

/// The topic of the internal event that releases one call.
pub const TOPIC: &str = "operator-release";

/// The reason the closing CDR event of a released call carries.
pub const REASON: &str = "operator_release";

/// The outcome of the release a drain's grace sends.
pub const AT_DRAIN_GRACE: &str = "drain-grace";

/// The reason the closing CDR event of a call released at a drain's grace
/// carries.
pub const DRAIN_GRACE_REASON: &str = "drain_grace";

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
    let call = &ctx.call;
    let reason = match ctx.event {
        CallEvent::InternalEvent { outcome, .. } if outcome == AT_DRAIN_GRACE => DRAIN_GRACE_REASON,
        _ => REASON,
    };
    let record = RuleAction::AddCdrEvent {
        event_type: CdrEventType::Bye,
        leg_id: call.a_leg().leg_id.clone(),
        status_code: None,
        reason: Some(reason.into()),
    };
    let mut actions = Vec::new();
    if call.caller_answered() {
        actions.extend(
            still_owed_bare_ack(call).map(|leg_id| RuleAction::AckLeg { leg_id, body: None }),
        );
        // A ringing leg keeps its INVITE for the teardown's CANCEL.
        let let_go = std::iter::once(call.a_leg())
            .chain(call.b_legs().iter())
            .filter(|leg| !leg_is_resolved(leg) && !leg.invite_ringing());
        actions.extend(let_go.map(|leg| RuleAction::TerminateLeg {
            leg_id: leg.leg_id.clone(),
            end: LegEnd::Abandon,
        }));
        actions.push(record);
    } else {
        actions.push(record);
        actions.push(RuleAction::RespondToALeg {
            status: 503,
            reason: "Service Unavailable".into(),
            header_updates: vec![],
            contacts: vec![],
        });
    }
    actions.push(RuleAction::BeginTermination {
        reason: Some(reason.into()),
        cause: TerminationCause::OperatorRelease,
        by_leg: None,
    });
    Some(RuleHandleResult::new(actions))
}
