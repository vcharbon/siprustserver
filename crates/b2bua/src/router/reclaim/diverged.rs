//! Two copies of one call that diverged (ADR-0055 §7): each made a compared
//! write the other never read. The node that meets the other copy ends the
//! call cleanly (§4) from the copy ranked higher on the call's lifecycle
//! (`call::helpers::lifecycle_behind`; the primary's on a tie), over the
//! union of both copies' dialogs by their tags (ADR-0041): each dialog at the
//! further of its two states, with that copy's INVITE exchange and the answer
//! it owes (RFC 3264 §4), and at the higher of each of its sequences, so the
//! teardown ACKs and BYEs a 2xx either copy took (RFC 3261 §13.2.2.4, §15) and
//! the BYE goes at the greater CSeq plus one; the caller answered when either
//! copy answered her; the CDR events of both. The union holds only the
//! request records whose transactions this node holds, so its teardown
//! answers them (RFC 3261 §15.1.2); the other node's teardown answers its
//! own.

use std::sync::Arc;

use b2bua_sdk::event::CallEvent;
use b2bua_sdk::model::RuleAction;
use call::helpers::{caller_answered, lifecycle_behind};
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
use crate::store::Enveloped;
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
    live: &Enveloped,
    incoming: Enveloped,
    now_ms: i64,
) {
    if live.call.state != CallModelState::Active {
        return;
    }
    let incoming_is_primary = matches!(direction, FlushDirection::Forward);
    let incoming_ranks = lifecycle_behind(&live.call, &incoming.call)
        || (!lifecycle_behind(&incoming.call, &live.call) && incoming_is_primary);
    let by = &ctx.relay_owner;
    let union = if incoming_ranks {
        union_of(incoming, &live.call, &live.call, by).merge_exempt(live)
    } else {
        union_of(live.clone(), &incoming.call, &live.call, by).merge_exempt(&incoming)
    };
    end_from(ctx, call_ref, &live.call, union, now_ms).await;
}

/// The other owner's ending copy `ending`, ranked higher than the `live`
/// copy it diverged from, ends the call over the union of both: when a
/// callee dialog only this node holds is still up — one `ending` lacks, or
/// holds less far along — or a request only this node relays or holds is
/// still open, this node's teardown ends it and the call discharges once it
/// has; otherwise the ending stands alone.
/// Returns whether a teardown ran here.
pub(super) async fn end_over_an_ending(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    live: &Enveloped,
    ending: &Enveloped,
    now_ms: i64,
) -> bool {
    let by = &ctx.relay_owner;
    if !only_here_to_end(&live.call, &ending.call, by) {
        return false;
    }
    let mut union = union_of(ending.clone(), &live.call, &live.call, by).merge_exempt(live);
    union.call.state = CallModelState::Terminating;
    end_from(ctx, call_ref, &live.call, union, now_ms).await;
    true
}

/// Install `union` as the live copy and end the call from it, counted.
async fn end_from(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    live: &Call,
    mut union: Enveloped,
    now_ms: i64,
) {
    ctx.metrics.bump_repl_copies_diverged();
    resync_timers(ctx, call_ref, live, &mut union.call, now_ms).await;
    ctx.state.update_enveloped(union.clone());
    let owners = ctx.owners_of(&union);
    end_cleanly(ctx, call_ref, &union.call, &owners, now_ms).await;
}

/// Whether `live` holds what `ending` cannot end: an early or confirmed
/// callee dialog `ending` does not hold as far along (a 2xx only `live`
/// took is ACKed and BYEd here), or a relayed or held request the worker
/// `by` keeps open. A dialled INVITE no dialog answered yet is left
/// to its own transaction (RFC 3261 §9.1: no CANCEL before a provisional).
fn only_here_to_end(live: &Call, ending: &Call, by: &RelayOwner) -> bool {
    let up = |d: &Dialog| matches!(d.state, DialogState::Early | DialogState::Confirmed);
    let leg_only_here = live.b_legs.iter().any(|leg| {
        let theirs =
            ending.b_legs.iter().find(|l| l.leg_id == leg.leg_id && l.call_id == leg.call_id);
        let held_there = |d: &Dialog| {
            theirs.is_some_and(|t| {
                t.dialogs
                    .by_tags(&d.our_tag, &d.peer_tag)
                    .is_some_and(|t| rank(&t.state) >= rank(&d.state))
            })
        };
        leg.dialogs.iter().any(|d| up(d) && !held_there(d))
    });
    let mine = |r: &RequestRecord| r.relayed_by == *by && !r.is_own();
    let open_here = std::iter::once(&live.a_leg).chain(live.b_legs.iter()).any(|leg| {
        leg.dialogs.iter().any(|d| {
            d.requests.iter().any(mine) || call::helpers::invite_relay(d).is_some_and(mine)
        })
    });
    leg_only_here || open_here
}

/// `base`, in its envelope, with every dialog of `other` it lacks, by the leg
/// and the two tags; a dialog both hold at the further of its two states with
/// that copy's INVITE exchange and owed answer, the higher of each sequence,
/// and the remote target and route set of the copy that read the peer's later request (a
/// target refresh is the peer's request, RFC 3261 §12.2); a leg only `other`
/// dialled; the caller answered when `other` answered her, and every early
/// dialog of hers ended once she took a final. A leg id names one leg in both
/// copies: two copies acting at once mint from distinct leg-id spaces
/// (`call::LegSpace`). The request records of the union are those of `live`
/// the worker `by` made: this node's teardown answers its own transactions,
/// the other node's teardown its own.
fn union_of(base: Enveloped, other: &Call, live: &Call, by: &RelayOwner) -> Enveloped {
    let Enveloped { call: mut base, topology } = base;
    union_leg(&mut base.a_leg, &other.a_leg);
    for theirs in &other.b_legs {
        match base.b_legs.iter_mut().find(|l| l.leg_id == theirs.leg_id) {
            Some(ours) => union_leg(ours, theirs),
            None => base.b_legs.push(theirs.clone()),
        }
    }
    if caller_answered(other) && !caller_answered(&base) {
        *base.a_leg.invite_final_sent_mut() = other.a_leg.invite_final_sent();
    }
    // The caller's final ends every early dialog of her INVITE (RFC 3261
    // §12.1, §13.2.2.4), the other copy's included.
    match base.a_leg.invite_final_sent() {
        Some(status) if status >= 300 => {
            call::helpers::reject_early_dialogs(&mut base.a_leg, status)
        }
        Some(_) => call::helpers::retire_early_dialogs(&mut base.a_leg),
        None => {}
    }
    Enveloped::new(own_records(base, live, by), topology)
}

/// `call` with, on each dialog, the open request records of the same dialog
/// of `live` the worker `by` made and no other: an INVITE relay another
/// worker made is left to it.
pub(super) fn own_records(mut call: Call, live: &Call, by: &RelayOwner) -> Call {
    let mine = |r: &RequestRecord| r.relayed_by == *by;
    let legs = std::iter::once(&mut call.a_leg).chain(call.b_legs.iter_mut());
    for leg in legs {
        let theirs =
            call::helpers::find_leg(live, &leg.leg_id).filter(|l| l.call_id == leg.call_id);
        for d in leg.dialogs.iter_mut() {
            let held = theirs.and_then(|l| l.dialogs.by_tags(&d.our_tag, &d.peer_tag));
            d.requests = held
                .map(|h| h.requests.iter().filter(|r| mine(r)).cloned().collect())
                .unwrap_or_default();
            let relay = held.and_then(call::helpers::invite_relay).filter(|r| mine(r));
            if let Some(relay) = relay {
                d.sent_invite = Some(relay.clone());
            } else if call::helpers::invite_relay(d).is_some_and(|r| !mine(r)) {
                d.sent_invite = None;
            }
        }
    }
    call
}

fn union_leg(ours: &mut Leg, theirs: &Leg) {
    if ours.call_id != theirs.call_id {
        return;
    }
    for d in theirs.dialogs.iter() {
        match ours.dialogs.by_tags_mut(&d.our_tag, &d.peer_tag) {
            Some(o) => union_dialog(o, d),
            None => ours.dialogs.insert(d.clone()),
        }
    }
}

fn union_dialog(ours: &mut Dialog, theirs: &Dialog) {
    if rank(&theirs.state) > rank(&ours.state) {
        ours.state = theirs.state.clone();
        ours.sent_invite.clone_from(&theirs.sent_invite);
        ours.owed_answer.clone_from(&theirs.owed_answer);
    }
    if theirs.peer_cseq > ours.peer_cseq {
        ours.remote_target = theirs.remote_target.clone();
        ours.route_set = theirs.route_set.clone();
    }
    ours.our_cseq = ours.our_cseq.max(theirs.our_cseq);
    ours.peer_cseq = ours.peer_cseq.max(theirs.peer_cseq);
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
async fn end_cleanly(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    call: &Call,
    owners: &crate::owners::Owners,
    now_ms: i64,
) {
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
    let mut exec = ActionExecutor { config: &ctx.config, owners, turn: &mut turn, now_ms };
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
