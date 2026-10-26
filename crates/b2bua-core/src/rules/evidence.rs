//! Evidence the call record cannot explain (ADR-0055 §4): a message that
//! proves the call took a step its record never saw. On a single node only a
//! broken or forged peer sends one. The triggers are closed:
//!
//! 1. a final in a dialog whose CSeq is above the dialog's local sequence,
//!    any method, no request record answering it (`evidence-final`);
//! 2. a final at or below the local sequence under a branch of ours no
//!    request record holds, at the CSeq of a request the record holds open
//!    under another branch: this side re-used a number another owner had
//!    sent, whose request it answers (`evidence-final`);
//! 3. an ACK naming a dialog by its To-tag and an INVITE on a leg for which
//!    the record holds no final sent (`evidence-ack`); the router passes the
//!    caller's own only on a call a copy served;
//! 4. a BYE from the callee on a dialog the record holds as early: RFC 3261
//!    §15 forbids it before a 2xx (`evidence-early-bye`, on a call a copy
//!    served; on one node served it is a broken callee and `relay-bye`'s);
//! 5. a 2xx whose via-extension names a leg the record lacks
//!    (`evidence-unknown-leg-2xx`, ADR-0055 §12).
//!
//! Any other final no request record holds is a stray, discarded or re-ACKed
//! as its rule reads it (§17.1.1.2, §18.1.2).
//!
//! The rule sends what RFC 3261 requires for the message and ends the call
//! over every dialog the record or the evidence names
//! ([`RuleAction::EndOnEvidence`], then the teardown); a caller still
//! unanswered when triggers 4 and 5 prove a callee answered is held possibly
//! confirmed ([`super::actions::possibly_confirmed`]). `re-ack-evidence`
//! sends the retained ACK again for a repeat of an evidence final's non-2xx,
//! which no transaction here absorbs (§17.1.1.2).

use call::{CallModelState, CdrEventType, DialogState, Direction, LegState, TerminationCause};

use b2bua_sdk::model::{
    LegEnd, Match, RuleAction, RuleContext, RuleDefinition, RuleHandleResult, CORE_LAYER,
};

/// The CDR reason of a call ended on evidence.
const EVIDENCE: &str = "evidence";

/// The evidence rules: their own stage, read before every rule of the chain
/// (ADR-0055 §4), so no service sees or pre-empts a message only a lost write
/// explains. Trigger 4 is a rule only on a call another owner served
/// (`copy_served`); on a call one node served the callee's BYE is
/// `relay-bye`'s.
pub fn evidence_rules(copy_served: bool) -> Vec<RuleDefinition> {
    let mut rules = vec![
        RuleDefinition::core(
            "evidence-final",
            CORE_LAYER,
            &["handle-481"],
            Match::response().filter(final_beyond_record),
            end_on_evidence,
        ),
        RuleDefinition::core(
            "evidence-ack",
            CORE_LAYER,
            &["relay-ack"],
            Match::request().method("ACK").filter(ack_of_unanswered_invite),
            end_on_evidence,
        ),
        RuleDefinition::core(
            "evidence-early-bye",
            CORE_LAYER,
            &["relay-bye"],
            Match::request()
                .method("BYE")
                .direction(Direction::FromB)
                .call_state(CallModelState::Active)
                .filter(callee_bye_on_early_dialog),
            |ctx| {
                let leg_id = ctx.source_leg_id.to_string();
                let mut actions = vec![
                    RuleAction::Respond {
                        status: 200,
                        reason: "OK".into(),
                        body: vec![],
                        content_type: None,
                    },
                    RuleAction::TerminateLeg { leg_id, end: LegEnd::PeerByeAnswered },
                ];
                actions.extend(end_on_evidence(ctx)?.actions);
                Some(RuleHandleResult::new(actions))
            },
        ),
        RuleDefinition::core(
            "evidence-unknown-leg-2xx",
            CORE_LAYER,
            &[],
            Match::response().method("INVITE").status_class(2).filter(names_unknown_leg),
            end_on_evidence,
        ),
        RuleDefinition::core(
            "re-ack-evidence",
            CORE_LAYER,
            &["absorb-stale-failure", "handle-481"],
            Match::response()
                .method("INVITE")
                .leg_states(&[LegState::Confirmed, LegState::Terminated])
                .filter(repeated_evidence_failure),
            |ctx| {
                let leg_id = ctx.source_leg_id.to_string();
                Some(RuleHandleResult::new(vec![RuleAction::AckLeg { leg_id, body: None }]))
            },
        ),
    ];
    rules.push(RuleDefinition::core(
        "evidence-reinvite",
        CORE_LAYER,
        &["reinvite-glare", "relay-reinvite"],
        Match::request().method("INVITE").filter(reinvite_of_unanswered_caller),
        end_on_evidence,
    ));
    let served_only = ["evidence-early-bye", "evidence-reinvite"];
    rules.retain(|r| copy_served || !served_only.contains(&r.id));
    rules
}

/// Trigger 6, on a call another owner may have answered: the caller's
/// re-INVITE naming a dialog by its To-tag while the record holds her INVITE
/// unanswered. A re-INVITE rides a confirmed dialog only (RFC 3261 §14.1;
/// RFC 3311 §1), so it proves her 2xx as her ACK does (trigger 3).
fn reinvite_of_unanswered_caller(ctx: &RuleContext) -> bool {
    let Some(req) = ctx.request() else { return false };
    let a_leg = ctx.call.a_leg();
    ctx.source_leg_id == a_leg.leg_id
        && req.to().tag().is_some()
        && req.cseq().seq() > ctx.call.a_leg_invite().cseq
        && a_leg.invite_final_sent().is_none()
}

/// Triggers 1 and 2: a final no request record answers, in a dialog whose
/// local sequence is below its CSeq, or at a CSeq the record holds open for a
/// request of its own under another branch.
fn final_beyond_record(ctx: &RuleContext) -> bool {
    let (Some(resp), Some(dialog)) = (ctx.response(), ctx.dialog()) else { return false };
    if resp.status() < 200 || ctx.resolved.record.is_some() {
        return false;
    }
    let cseq = i64::from(resp.cseq().seq());
    cseq > dialog.our_cseq || reused_cseq(dialog, cseq, resp.top_via().branch())
}

/// Whether `dialog` holds open a request it sent at `cseq` under a branch
/// other than `branch`.
fn reused_cseq(dialog: &call::Dialog, cseq: i64, branch: Option<&str>) -> bool {
    let other = |r: &call::RequestRecord| {
        r.outbound_cseq == cseq && r.branch().is_some_and(|b| Some(b) != branch)
    };
    let open_invite = dialog.sent_invite.as_ref().is_some_and(|r| r.is_sent() && other(r))
        && !call::helpers::invite_taken(dialog);
    open_invite || dialog.requests.iter().any(|r| r.awaits_final() && other(r))
}

/// Trigger 3: an ACK naming a dialog by its To-tag and an INVITE of the leg
/// it arrives on for which the record holds no final sent — the caller's
/// INVITE unanswered, a re-INVITE the record never saw, or one it holds open
/// (relayed and untaken, or held).
fn ack_of_unanswered_invite(ctx: &RuleContext) -> bool {
    let (Some(req), Some(leg)) = (ctx.request(), ctx.source_leg()) else { return false };
    let cseq = i64::from(req.cseq().seq());
    let a_leg = ctx.call.a_leg();
    if leg.leg_id == a_leg.leg_id && cseq == i64::from(ctx.call.a_leg_invite().cseq) {
        return a_leg.invite_final_sent().is_none() && req.to().tag().is_some();
    }
    let Some(dialog) = ctx.dialog() else { return false };
    if dialog.state != DialogState::Confirmed {
        return false;
    }
    if dialog.peer_cseq.is_none_or(|last| cseq > last) {
        return true;
    }
    held_invite(dialog, cseq) || relayed_open(ctx, cseq)
}

/// Whether `dialog` holds the INVITE of CSeq `cseq` it received unanswered.
fn held_invite(dialog: &call::Dialog, cseq: i64) -> bool {
    dialog.requests.iter().any(|r| r.is_invite() && r.inbound_cseq == cseq && r.is_held())
}

/// Whether the INVITE of CSeq `cseq` the source leg's peer sent was relayed
/// to the other leg and no final settled it there.
fn relayed_open(ctx: &RuleContext, cseq: i64) -> bool {
    let Some(peer) = ctx.peer_dialog() else { return false };
    call::helpers::invite_relay(peer).is_some_and(|r| r.inbound_cseq == cseq)
}

/// Trigger 4, on a call another owner may have answered: a BYE from the
/// callee on a dialog the record holds as early (RFC 3261 §15).
fn callee_bye_on_early_dialog(ctx: &RuleContext) -> bool {
    ctx.dialog().is_some_and(|d| d.state == DialogState::Early)
        && ctx.call.a_leg().invite_final_sent().is_none()
}

/// Trigger 5: a 2xx whose verified via-extension names a leg the record
/// lacks.
fn names_unknown_leg(ctx: &RuleContext) -> bool {
    let Some(resp) = ctx.response() else { return false };
    let Some(ext) = crate::stack_identity::read_via_extension(
        resp.top_via(),
        resp.cseq().method().as_str(),
        ctx.config.cluster_key.as_ref(),
    ) else {
        return false;
    };
    ext.leg != ctx.call.a_leg().leg_id && !ctx.call.b_legs().iter().any(|l| l.leg_id == ext.leg)
}

/// Whether the current response repeats the non-2xx of an evidence final: the
/// dialog's INVITE record holds no image of the INVITE and the ACK it kept
/// names this CSeq.
fn repeated_evidence_failure(ctx: &RuleContext) -> bool {
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

fn end_on_evidence(ctx: &RuleContext) -> Option<RuleHandleResult> {
    let leg = ctx.source_leg_id.to_string();
    Some(RuleHandleResult::new(vec![
        RuleAction::EndOnEvidence { leg_id: leg.clone() },
        RuleAction::AddCdrEvent {
            event_type: CdrEventType::Bye,
            leg_id: leg.clone(),
            status_code: None,
            reason: Some(EVIDENCE.into()),
        },
        RuleAction::BeginTermination {
            reason: Some(EVIDENCE.into()),
            cause: TerminationCause::RemoteFinal,
            by_leg: Some(leg),
        },
    ]))
}
