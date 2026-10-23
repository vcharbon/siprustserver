//! `ack-lost-round` (ADR-0047): an INVITE final on a confirmed dialog whose
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
/// `re-ack-lost-round` sends the retained ACK again for a repeat of the
/// round's non-2xx, which no transaction here absorbs (§17.1.1.2); a repeated
/// 2xx is `re-ack-retransmitted-2xx`'s.
pub fn lost_round_rules() -> Vec<RuleDefinition> {
    vec![
        RuleDefinition::core(
            "ack-lost-round",
            CORE_LAYER,
            &["handle-481"],
            Match::response()
                .method("INVITE")
                .leg_states(&[LegState::Confirmed])
                .filter(lost_round),
            ack_lost_round,
        ),
        RuleDefinition::core(
            "re-ack-lost-round",
            CORE_LAYER,
            &["absorb-stale-failure", "handle-481"],
            Match::response()
                .method("INVITE")
                .leg_states(&[LegState::Confirmed, LegState::Terminated])
                .filter(repeated_lost_failure),
            |ctx| {
                let leg_id = ctx.source_leg_id.to_string();
                Some(RuleHandleResult::new(vec![RuleAction::AckLeg { leg_id, body: None }]))
            },
        ),
    ]
}

/// Whether the current response repeats the non-2xx of a lost round: the
/// dialog's INVITE record holds no image of the INVITE and the ACK it kept
/// names this CSeq.
fn repeated_lost_failure(ctx: &RuleContext) -> bool {
    let (Some(resp), Some(dialog), Some(leg)) = (ctx.response(), ctx.dialog(), ctx.source_leg())
    else {
        return false;
    };
    resp.status() >= 300
        && call::helpers::sent_invite_image(leg, dialog).is_none()
        && call::helpers::emitted_ack(dialog).is_some()
        && crate::rules::relay::acked_invite_cseq(leg, dialog) == Some(resp.cseq().seq())
        && ctx.relayed_pending().is_none()
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
