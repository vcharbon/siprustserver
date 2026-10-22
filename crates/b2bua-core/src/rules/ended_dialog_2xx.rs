//! `ack-2xx-on-ended-dialog` (RFC 3261 §13.2.2.4): a 2xx to the INVITE this
//! stack last sent on a dialog a BYE has since ended, or is ending, still draws
//! its ACK — the re-INVITE's transaction outlives the dialog's usage (RFC 5407
//! §2), and its answerer retransmits the 2xx until it is acknowledged. An offer
//! the 2xx makes is answered with every stream rejected (RFC 3264 §6), in the
//! session this stack stated on the dialog (§8): the session it would describe
//! is over. The dialog's BYE is already sent or
//! answered, so no second one follows; a repeat of the 2xx is
//! `re-ack-retransmitted-2xx`'s, with the same ACK.

use call::DialogState;

use b2bua_sdk::model::{
    Body, Match, RuleAction, RuleContext, RuleDefinition, RuleHandleResult, CORE_LAYER,
};

use crate::rules::relay;

/// The rule family: CORE, registered before the generic core set. Disjoint from
/// `re-ack-retransmitted-2xx` (an ACK already sent), `release-fork-straggler-2xx`
/// (a dialog no 2xx established) and `relay-reinvite-response` (a relayed
/// re-INVITE's 2xx, owed to its originator). A 2xx whose ACK the originator of
/// a relayed INVITE still owes is hers to acknowledge, whoever's BYE ended the
/// dialog meanwhile, so the rule leaves it alone.
pub fn ended_dialog_2xx_rules() -> Vec<RuleDefinition> {
    vec![RuleDefinition::core(
        "ack-2xx-on-ended-dialog",
        CORE_LAYER,
        &[],
        Match::response().method("INVITE").status_class(2).filter(answers_our_invite_after_bye),
        ack_2xx,
    )]
}

/// Whether the current 2xx is the first to the INVITE this stack last sent on a
/// dialog ending or ended by a BYE, with no relay pending for it and no relayed
/// ACK awaited for it.
fn answers_our_invite_after_bye(ctx: &RuleContext) -> bool {
    let (Some(resp), Some(dialog)) = (ctx.response(), ctx.dialog()) else { return false };
    dialog.was_established()
        && dialog.state != DialogState::Confirmed
        && dialog.pending_invite_txn.is_some()
        && dialog.ack_branch.is_none()
        && dialog.awaited_ack_cseq.is_none()
        && resp.to().tag() == Some(dialog.peer_tag.as_str())
        && relay::acked_invite_cseq(dialog) == Some(resp.cseq().seq())
        && ctx.relayed_pending().is_none()
}

fn ack_2xx(ctx: &RuleContext) -> Option<RuleHandleResult> {
    let resp = ctx.response()?;
    let dialog = ctx.dialog()?;
    let options = sip_message::BuildHeldSdpOptions {
        local_ip: ctx.config.sip_local_ip.clone(),
        now_ms: ctx.now_ms,
    };
    // The answer continues the session this stack stated on the dialog (RFC
    // 3264 §8).
    let origin = ctx
        .call
        .b_legs()
        .iter()
        .find(|l| l.leg_id == ctx.source_leg_id)
        .and_then(|l| relay::next_origin_in_dialog(l, &dialog.peer_tag));
    let answer = (!relay::acked_invite_carries_offer(dialog))
        .then(|| resp.sdp())
        .flatten()
        .and_then(|offer| sip_message::sdp_answer::reject_offer(offer, &options))
        .and_then(|bytes| relay::in_dialog_session(bytes, origin.as_deref()))
        .map(|bytes| Body::own(bytes, None));
    let leg_id = ctx.source_leg_id.to_string();
    Some(RuleHandleResult::new(vec![RuleAction::AckLeg { leg_id, body: answer }]))
}
