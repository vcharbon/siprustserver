//! The other owner's ending of the call, folded into the record that ends it
//! here (ADR-0014 §2, ADR-0020 X3): its terminal body is discharged through
//! the one enforcement funnel, and every re-INVITE the record relayed that
//! the ending body never read is answered first.

use std::sync::Arc;

use b2bua_sdk::event::CallEvent;
use b2bua_sdk::model::RuleAction;
use call::helpers::raise_sequences;
use call::{Call, Direction};

use super::super::interpret::process_result;
use super::super::RouterCtx;
use crate::effects::HandlerResult;
use crate::rules::{ActionExecutor, RuleCall, RuleContext};

/// Discharge an already-`Terminated` body (a backup's deferred terminal, folded
/// into our live map or reclaimed on reboot) through the ONE enforcement funnel.
/// `before` is the live (non-terminal) snapshot so `enforce`'s `became_terminated`
/// edge fires — `discharge_result`/`discharge_as_own` cannot be reused here because
/// they synthesise the terminal from a NON-terminal call, whereas this body is
/// already Terminated (the edge would be vacuous and the ObligationSet would never
/// settle the CDR / limiter / RemoveCall). The CDR + limiter release + propagated
/// delete all ride `process_result`, exactly as a primary-served BYE would. The
/// discharged body keeps the higher of both views' dialog sequences.
///
/// The record may hold a round the ending body never read: a re-INVITE it
/// relayed whose client transaction it alone holds. The call's end answers it
/// as this stack's own teardown does (RFC 3261 §15.1.2: a 487 to its
/// originator, a CANCEL to its target), so no originator is left without a
/// final.
pub(super) async fn discharge_folded_terminal(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    before: &Call,
    terminal: Call,
    now_ms: i64,
) {
    let mut result = HandlerResult::new(raise_sequences(terminal, before));
    answer_the_records_rounds(ctx, call_ref, before, &mut result, now_ms);
    let result = crate::rules::invariants::enforce(
        &ctx.obligations,
        before,
        crate::rules::invariants::finalize(result),
        now_ms,
        // Folded already-terminal body: same off-the-wire contract as
        // `discharge_materialized_terminal`.
        crate::rules::invariants::UnansweredCaller::Leave,
    );
    process_result(ctx, call_ref, result, now_ms, &"the discharge of a folded terminal body", None)
        .await;
}

/// The effects answering every re-INVITE relay `record` holds open that the
/// ending body `result` carries does not: the record's own rounds.
fn answer_the_records_rounds(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    record: &Call,
    result: &mut HandlerResult,
    now_ms: i64,
) {
    let actions: Vec<RuleAction> = open_rounds_alone(record, &result.call)
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
    let exec = ActionExecutor {
        config: &ctx.config,
        id_gen: &ctx.id_gen,
        now_ms,
        wire_faults: &ctx.wire_faults,
    };
    let answered = exec.execute(&actions, record, &rule_ctx).effects;
    result.effects.critical.extend(answered.critical);
    result.effects.outbound.extend(answered.outbound);
    result.effects.soft.extend(answered.soft);
    result.effects.buffered.extend(answered.buffered);
    result.effects.fire_and_forget.extend(answered.fire_and_forget);
}

/// `(leg, outbound CSeq)` of every re-INVITE relay `record` holds open, not
/// CANCELled, that `ending`'s same dialog does not hold.
fn open_rounds_alone(record: &Call, ending: &Call) -> Vec<(String, i64)> {
    let legs = || std::iter::once(&record.a_leg).chain(record.b_legs.iter());
    let mut out = Vec::new();
    for leg in legs() {
        let theirs = call::helpers::find_leg(ending, &leg.leg_id);
        for d in leg.dialogs.iter() {
            let held = theirs.and_then(|l| l.dialogs.by_tags(&d.our_tag, &d.peer_tag));
            for p in d.inbound_pending_requests.iter() {
                let invite = p.method.eq_ignore_ascii_case("INVITE") && !p.cancelled;
                let read = held.is_some_and(|h| {
                    h.inbound_pending_requests.iter().any(|t| t.outbound_cseq == p.outbound_cseq)
                });
                if invite && !read {
                    out.push((leg.leg_id.clone(), p.outbound_cseq));
                }
            }
        }
    }
    out
}
