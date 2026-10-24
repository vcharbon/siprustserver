//! Two copies of one call that diverged (ADR-0055 §7): each made a compared
//! write the other never read. The node that meets the other copy ends the
//! call cleanly (§4) from the copy ranked higher on the call's lifecycle
//! (`call::helpers::lifecycle_behind`; the primary's on a tie), over the
//! union of both copies' dialogs by their tags (ADR-0041): each dialog at the
//! further of its two states and at the higher of each of its sequences, so
//! the BYE goes at the greater CSeq plus one; the caller answered when either
//! copy answered her; the CDR events of both. The request records whose
//! transactions this node holds stay in the union, so its teardown answers
//! them (RFC 3261 §15.1.2); the other node's teardown answers its own.

use std::sync::Arc;

use b2bua_sdk::event::CallEvent;
use b2bua_sdk::model::RuleAction;
use call::helpers::{caller_answered, lifecycle_behind, merge_exempt};
use call::TerminationCause;
use call::{
    Call, CallModelState, CdrEventType, Dialog, DialogEnd, DialogState, Direction, EndingBy, Leg,
    RelayOwner, RequestRecord,
};

use super::super::interpret::process_result;
use super::super::RouterCtx;
use super::meeting::resync_timers;
use super::FlushDirection;
use crate::rules::{ActionExecutor, RuleCall, RuleContext};
use crate::turn_scope::TurnScope;

/// The reason the CDR event and the termination of a call ended on two
/// diverged copies carry.
const DIVERGED: &str = "diverged";

/// End the call cleanly from the union of the `live` copy and the other
/// owner's `incoming` one, which diverged; `direction` says whose `incoming`
/// is. A live copy already ending is left to its teardown. The union becomes
/// the live copy, its ledger followed by the timer service, and the
/// teardown runs on it as a turn of its own: every confirmed dialog BYEd,
/// a ringing leg CANCELled, an unanswered caller answered.
pub(super) async fn end_diverged(
    ctx: &Arc<RouterCtx>,
    direction: FlushDirection,
    call_ref: &str,
    live: &Call,
    incoming: Call,
    now_ms: i64,
) {
    if live.state != CallModelState::Active {
        return;
    }
    let incoming_is_primary = matches!(direction, FlushDirection::Forward);
    let incoming_ranks = lifecycle_behind(live, &incoming)
        || (!lifecycle_behind(&incoming, live) && incoming_is_primary);
    let by = ctx.config.relay_owner();
    let mut union = if incoming_ranks {
        merge_exempt(union_of(incoming.clone(), live, Some(&by)), live)
    } else {
        merge_exempt(union_of(live.clone(), &incoming, None), &incoming)
    };
    resync_timers(ctx, call_ref, live, &mut union, now_ms).await;
    ctx.state.update_quiet(union.clone());
    end_cleanly(ctx, call_ref, &union, now_ms).await;
}

/// `base` with every dialog of `other` it lacks, by the leg and the two tags;
/// a dialog both hold at the further of its two states and the higher of
/// each sequence, with the open request records of `other` the worker
/// `carry` made, when it names one; a leg only `other` dialled; the caller
/// answered when `other` answered her. A leg both name under one id with
/// another Call-ID is not the same leg, and stays `base`'s alone.
fn union_of(mut base: Call, other: &Call, carry: Option<&RelayOwner>) -> Call {
    union_leg(&mut base.a_leg, &other.a_leg, carry);
    for theirs in &other.b_legs {
        match base.b_legs.iter_mut().find(|l| l.leg_id == theirs.leg_id) {
            Some(ours) => union_leg(ours, theirs, carry),
            None => base.b_legs.push(theirs.clone()),
        }
    }
    if caller_answered(other) && !caller_answered(&base) {
        *base.a_leg.invite_final_sent_mut() = other.a_leg.invite_final_sent();
    }
    base
}

fn union_leg(ours: &mut Leg, theirs: &Leg, carry: Option<&RelayOwner>) {
    if ours.call_id != theirs.call_id {
        return;
    }
    for d in theirs.dialogs.iter() {
        match ours.dialogs.by_tags_mut(&d.our_tag, &d.peer_tag) {
            Some(o) => union_dialog(o, d, carry),
            None => ours.dialogs.insert(d.clone()),
        }
    }
}

fn union_dialog(ours: &mut Dialog, theirs: &Dialog, carry: Option<&RelayOwner>) {
    if rank(&theirs.state) > rank(&ours.state) {
        ours.state = theirs.state.clone();
    }
    ours.our_cseq = ours.our_cseq.max(theirs.our_cseq);
    ours.peer_cseq = ours.peer_cseq.max(theirs.peer_cseq);
    let Some(by) = carry else { return };
    let mine = |r: &RequestRecord| r.relayed_by == *by;
    for record in theirs.requests.iter().filter(|r| mine(r)) {
        if !ours.requests.contains(record) {
            ours.requests.push(record.clone());
        }
    }
    let open_relay = call::helpers::invite_relay(theirs).filter(|r| r.relayed_by == *by);
    if let Some(relay) = open_relay {
        let later =
            ours.sent_invite.as_ref().is_some_and(|r| r.outbound_cseq > relay.outbound_cseq);
        if !later {
            ours.sent_invite = theirs.sent_invite.clone();
        }
    }
}

/// How far along its life a dialog stands: early; ended while early (let go,
/// CANCELled, refused), which a 2xx still overrides (RFC 3261 §9.2, §13.2.2.4);
/// confirmed; its BYE sent; ended after a BYE.
fn rank(state: &DialogState) -> u8 {
    match state {
        DialogState::Early => 0,
        DialogState::Retired(_)
        | DialogState::Ending(EndingBy::Cancel)
        | DialogState::Ended(DialogEnd::Rejected(_)) => 1,
        DialogState::Confirmed => 2,
        DialogState::Ending(EndingBy::Bye) => 3,
        DialogState::Ended(_) => 4,
    }
}

/// The teardown of `call` as a turn of its own: the closing CDR event and the
/// termination, through the one enforcement funnel, the caller answered if
/// she still waits on this node.
async fn end_cleanly(ctx: &Arc<RouterCtx>, call_ref: &str, call: &Call, now_ms: i64) {
    let actions = [
        RuleAction::AddCdrEvent {
            event_type: CdrEventType::Bye,
            leg_id: call.a_leg.leg_id.clone(),
            status_code: None,
            reason: Some(DIVERGED.into()),
        },
        RuleAction::BeginTermination {
            reason: Some(DIVERGED.into()),
            cause: TerminationCause::Supervisor,
            by_leg: None,
        },
    ];
    let event = CallEvent::InternalEvent {
        call_ref: call_ref.to_string(),
        topic: DIVERGED.into(),
        outcome: String::new(),
        payload: serde_json::Value::Null,
        body: Vec::new(),
        incarnation: None,
    };
    let rule_ctx = RuleContext {
        call: RuleCall::new(call),
        call_ref,
        event: &event,
        source_leg_id: "",
        direction: Direction::FromA,
        now_ms,
        config: &ctx.config,
        discharged: None,
        resolved: &call::helpers::UNRESOLVED,
    };
    let mut turn = TurnScope::of(ctx.id_gen.turn()).with_faults(ctx.wire_faults.snapshot());
    let mut exec = ActionExecutor { config: &ctx.config, turn: &mut turn, now_ms };
    let result = exec.execute(&actions, call, &rule_ctx);
    let result = crate::rules::invariants::enforce(
        &ctx.obligations,
        call,
        crate::rules::invariants::finalize(result),
        now_ms,
        crate::rules::invariants::UnansweredCaller::Answer(&ctx.config.minted_final_advertisement),
        &mut turn.notes,
    );
    super::super::notes::drain(ctx, call_ref, &mut turn);
    process_result(ctx, call_ref, result, now_ms, &"the clean end of two diverged copies", None)
        .await;
}
