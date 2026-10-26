//! The other owner's ending of the call, met in the record that ends it
//! here (ADR-0055 §7, ADR-0020 X3): its terminal body is discharged through
//! the one enforcement funnel, and every re-INVITE this node relayed draws
//! one final for its originator.

use std::sync::Arc;

use b2bua_sdk::event::CallEvent;
use b2bua_sdk::model::RuleAction;
use call::helpers::{keep_dialogs, raise_sequences};
use call::{Call, Direction, RelayOwner, RequestRecord};

use super::super::interpret::process_result;
use super::super::materialise::relayed_server_key;
use super::super::RouterCtx;
use crate::effects::HandlerResult;
use crate::rules::{ActionExecutor, RuleCall, RuleContext};
use crate::store::Enveloped;
use crate::turn_scope::TurnScope;

/// Discharge an already-`Terminated` body (a backup's deferred terminal, met
/// in our live map or reclaimed on reboot) through the ONE enforcement funnel.
/// `before` is the live (non-terminal) snapshot so `enforce`'s `became_terminated`
/// edge fires — `discharge_result`/`discharge_as_own` cannot be reused here because
/// they synthesise the terminal from a NON-terminal call, whereas this body is
/// already Terminated (the edge would be vacuous and the ObligationSet would never
/// settle the CDR / limiter / RemoveCall). The CDR + limiter release + propagated
/// delete all ride `process_result`, exactly as a primary-served BYE would. The
/// discharged body keeps the higher of both views' dialog sequences, and the
/// dialogs only this node's copy holds.
///
/// Every re-INVITE this node relayed and the record holds open draws one
/// final for its originator (RFC 3261 §15.1.2), from the server transaction
/// this node holds:
/// - the ending body holds it CANCELled — the other copy's teardown answered
///   it — so this node relinquishes its server transaction with nothing sent;
///   its client transaction stays to absorb the target's final;
/// - otherwise this node answers it as its own teardown does: a 487 to its
///   originator, a CANCEL to its target.
///
/// A round the other copy relayed has its server transaction there, and its
/// ending answers it. The record takes the ending body's envelope.
pub(super) async fn discharge_met_ending(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    before: &Call,
    terminal: Enveloped,
    now_ms: i64,
) {
    let Enveloped { call: terminal, topology } = terminal;
    let by = &ctx.relay_owner;
    let (answered_there, ours) = own_open_rounds(before, &terminal, by);
    for key in answered_there.iter().filter_map(relayed_server_key) {
        let _ = ctx.txn.relinquish_server(key).await;
    }
    let mut result = HandlerResult::new(keep_dialogs(raise_sequences(terminal, before), before));
    let mut turn = TurnScope::of(ctx.id_gen.turn()).with_faults(ctx.wire_faults.snapshot());
    let owners = ctx.owners(call_ref);
    let rounds = Rounds { record: before, rounds: ours, owners: &owners };
    answer_the_records_rounds(ctx, call_ref, rounds, &mut result, now_ms, &mut turn);
    ctx.state.update_enveloped(Enveloped::new(before.clone(), topology));
    let result = crate::rules::invariants::enforce(
        &ctx.obligations,
        before,
        crate::rules::invariants::finalize(result),
        now_ms,
        // The other copy's already-terminal body: same off-the-wire contract as
        // `discharge_materialized_terminal`.
        crate::rules::invariants::UnansweredCaller::Leave,
        &mut turn.notes,
    );
    super::super::notes::drain(ctx, call_ref, &mut turn);
    process_result(
        ctx,
        call_ref,
        result,
        now_ms,
        &"the discharge of the other copy's ending",
        None,
    )
    .await;
}

/// The record's re-INVITE relays to answer — `(leg, outbound CSeq)` — and
/// the call's owners.
struct Rounds<'a> {
    record: &'a Call,
    rounds: Vec<(String, i64)>,
    owners: &'a crate::owners::Owners,
}

/// The effects answering the record's re-INVITE relays `rounds`: 487 to the
/// originator and CANCEL to the target.
fn answer_the_records_rounds(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    rounds: Rounds,
    result: &mut HandlerResult,
    now_ms: i64,
    turn: &mut TurnScope,
) {
    let Rounds { record, rounds, owners } = rounds;
    let actions: Vec<RuleAction> = rounds
        .into_iter()
        .map(|(leg_id, outbound_cseq)| RuleAction::RejectPendingReinvite {
            leg_id,
            outbound_cseq,
            status: 487,
            reason: "Request Terminated".into(),
        })
        .collect();
    if actions.is_empty() {
        return;
    }
    let event = CallEvent::InternalEvent {
        call_ref: call_ref.to_string(),
        topic: "folded-ending".into(),
        outcome: String::new(),
        payload: serde_json::Value::Null,
        body: Vec::new(),
        incarnation: None,
    };
    let rule_ctx = RuleContext {
        call: RuleCall::new(record),
        call_ref,
        event: &event,
        source_leg_id: "",
        direction: Direction::FromA,
        now_ms,
        config: &ctx.config,
        discharged: None,
        resolved: &call::helpers::UNRESOLVED,
    };
    let mut exec = ActionExecutor { config: &ctx.config, owners, turn, now_ms };
    let answered = exec.execute(&actions, record, &rule_ctx).effects;
    result.effects.critical.extend(answered.critical);
    result.effects.outbound.extend(answered.outbound);
    result.effects.soft.extend(answered.soft);
    result.effects.buffered.extend(answered.buffered);
    result.effects.fire_and_forget.extend(answered.fire_and_forget);
}

/// The re-INVITE relays worker `by` made that `record` holds open, not
/// CANCELled: those `ending` holds CANCELled, and the `(leg, outbound CSeq)`
/// of the others.
fn own_open_rounds(
    record: &Call,
    ending: &Call,
    by: &RelayOwner,
) -> (Vec<RequestRecord>, Vec<(String, i64)>) {
    let legs = || std::iter::once(&record.a_leg).chain(record.b_legs.iter());
    let (mut answered_there, mut ours) = (Vec::new(), Vec::new());
    for leg in legs() {
        let theirs = call::helpers::find_leg(ending, &leg.leg_id);
        for d in leg.dialogs.iter() {
            let held = theirs.and_then(|l| l.dialogs.by_tags(&d.our_tag, &d.peer_tag));
            let open =
                call::helpers::invite_relay(d).filter(|p| !p.cancelled && p.relayed_by == *by);
            if let Some(p) = open {
                let cancelled_there = held.is_some_and(|h| {
                    call::helpers::invite_relay(h)
                        .is_some_and(|t| t.outbound_cseq == p.outbound_cseq && t.cancelled)
                });
                if cancelled_there {
                    answered_there.push(p.clone());
                } else {
                    ours.push((leg.leg_id.clone(), p.outbound_cseq));
                }
            }
        }
    }
    (answered_there, ours)
}
