//! `ack-lost-round` (ADR-0046): an INVITE final on a confirmed dialog whose
//! CSeq is above the last this stack's record sent on that dialog, with no
//! relay pending for it, answers a round the record lost — the node that sent
//! it died before the round reached this copy. Nothing else claims it: no
//! transaction took it, and it is no repeat of a final the record holds. The
//! round is acknowledged (`AckLostRound`) and the call ended, with a BYE on
//! every leg: the request that opened the round on the other leg existed only
//! on the node that died, so its originator can be given no final for it, and
//! a session whose state no record holds is not carried on.

use call::{CdrEventType, DialogState, LegState, TerminationCause};

use b2bua_sdk::model::{
    Match, RuleAction, RuleContext, RuleDefinition, RuleHandleResult, CORE_LAYER,
};

/// The CDR reason of a call ended on a lost round.
const LOST_ROUND: &str = "lost_round";

/// The lost-round rule family: CORE, registered before the generic core set.
pub fn lost_round_rules() -> Vec<RuleDefinition> {
    vec![RuleDefinition::core(
        "ack-lost-round",
        CORE_LAYER,
        &["handle-481"],
        Match::response().method("INVITE").leg_states(&[LegState::Confirmed]).filter(lost_round),
        ack_lost_round,
    )]
}

/// Whether the current response is a final to a round the record lost.
fn lost_round(ctx: &RuleContext) -> bool {
    let (Some(resp), Some(dialog)) = (ctx.response(), ctx.dialog()) else { return false };
    resp.status() >= 200
        && dialog.state == DialogState::Confirmed
        && i64::from(resp.cseq().seq()) > dialog.our_cseq
        && ctx.relayed_pending().is_none()
}

fn ack_lost_round(ctx: &RuleContext) -> Option<RuleHandleResult> {
    let leg = ctx.source_leg_id.to_string();
    Some(RuleHandleResult::new(vec![
        RuleAction::AckLostRound { leg_id: leg.clone() },
        RuleAction::AddCdrEvent {
            event_type: CdrEventType::Bye,
            leg_id: leg.clone(),
            status_code: None,
            reason: Some(LOST_ROUND.into()),
        },
        RuleAction::BeginTermination {
            reason: Some(LOST_ROUND.into()),
            cause: TerminationCause::RemoteFinal,
            by_leg: Some(leg),
        },
    ]))
}
