//! The initial-INVITE turn: the store-fault probe, the identity guard, then
//! the new call built and ruled.

use std::net::SocketAddr;
use std::sync::Arc;

use call::{Call, TerminationCause};
use sip_ids::TurnIds;
use sip_message::SipRequest;

use super::admit::UnbornCall;
use super::reply_to::reply_to;
use super::responses::{build_merged_482, build_retry_later_500, build_unborn_500};
use super::topology_cookie::topology_from_cookie;
use super::RouterCtx;
use crate::admission::{class_of, Class};
use crate::effects::{HandlerEffects, HandlerResult};
use crate::initial_consult::{cancelled_setup, handle_initial_invite, Consulted};
use crate::initial_invite::build_initial_call;
use crate::new_calls::Refusal;
use crate::repl::ReadinessState;
use crate::rules::lifecycle_delivery::{close_turn, TurnStart};
use crate::rules::ActionExecutor;
use crate::store::StoreFaultPoint;
use crate::turn_scope::TurnScope;

/// Outcome of the initial-INVITE turn.
pub(super) enum Turn {
    /// Answered on a call still here, which keeps its per-call ephemera.
    Consumed,
    /// A final went on the wire and no call is here; the caller tears down
    /// the per-call ephemera this dispatch created (`ReleaseKind::Orphan`).
    Shed,
    /// Admitted (or created-then-rejected): the handler result to interpret.
    Result(Box<HandlerResult>),
}

/// The initial-INVITE turn: the admission ladder's router rungs admitted it
/// at ingress (`super::admit`, ADR-0037); here the store-fault probe, the
/// identity guard, then build + rule the new call. A setup its caller
/// CANCELed before this turn ran is born and ruled by no one: no decision, no
/// limiter admit. The probe's 500, the identity guard's 500, the cancelled
/// setup, the cordon's 503 or the accept counts the INVITE's outcome once
/// ([`crate::new_calls`]); a copy is answered 482 and refused as a copy.
/// Every identifier the turn mints is drawn from `turn`; `datagram` is the
/// INVITE as it arrived, what a trace activation backfills its `sip.in` from.
#[allow(clippy::too_many_arguments)]
pub(super) async fn initial_invite_turn(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    req: &sip_message::SipRequest,
    src: SocketAddr,
    datagram: &sip_message::Bytes,
    now_ms: i64,
    unborn: Option<UnbornCall>,
    turn: &mut TurnScope,
) -> Turn {
    // ── Live-path store-fault probe (ADR-0023): initial INVITE ──────────────
    // The dialog-existence lookup (the `peek` identity guard below) is the
    // store read this INVITE depends on; a faulted store cannot answer "does
    // this dialog already exist", so the probe fires BEFORE the peek (its
    // answer is untrustworthy under a fault). Fail CLOSED: a final **500
    // Server Internal Error** through the INVITE server txn — superseding the
    // auto-100, composing with the ADR-0022 no-100-then-silence guarantee —
    // and NO call state is born.
    let class = class_of(req);
    let cseq = req.cseq().seq();
    // A turn with no admission hold is a copy, offered unjudged: never a new
    // call (`super::admit`).
    let copy = unborn.is_none();
    if ctx.store_faults.check(StoreFaultPoint::LiveInitialInvite).is_err() {
        let resp = build_unborn_500(&mut turn.ids, req, &ctx.config.minted_final_advertisement);
        if let Ok(resp) = resp {
            let _ = ctx.txn.send_response(resp, reply_to(req, src)).await;
        }
        ctx.metrics.bump_store_fault_rejected();
        if copy {
            ctx.metrics.new_calls().refuse_copy();
        } else {
            ctx.metrics.new_calls().reject(Refusal::StoreFault, class);
        }
        // A call resident on the identity (this INVITE a copy of it, or a new
        // request on it) owns the queue and keeps it.
        if ctx.state.peek(call_ref).is_none() {
            return Turn::Shed;
        }
        if !copy {
            ctx.state.clear_setup_cancelled(call_ref, cseq);
        }
        return Turn::Consumed;
    }

    // A copy is a request merged with the INVITE it copies (RFC 3261
    // §8.2.2.2): answered 482, it creates no call, as no call is born past
    // the admission ladder.
    let resident = ctx.state.peek(call_ref).is_some();
    if copy {
        let merged = build_merged_482(&mut turn.ids, req, &ctx.config.minted_final_advertisement);
        if let Ok(merged) = merged {
            let _ = ctx.txn.send_response(merged, reply_to(req, src)).await;
        }
        ctx.metrics.new_calls().refuse_copy();
        return if resident { Turn::Consumed } else { Turn::Shed };
    }
    // A new request on the identity of a call still here: one call per
    // identity, so the caller is told to retry once it is gone (§14.2).
    if resident {
        let retry_after = super::owed::OwedAnswer::of(ctx).jittered_retry_after_in(&mut turn.ids);
        let resp = build_retry_later_500(req, Some(turn.ids.new_tag()), retry_after);
        if let Ok(resp) = resp {
            let _ = ctx.txn.send_response(resp, reply_to(req, src)).await;
        }
        ctx.metrics.new_calls().reject(Refusal::IdentityInUse, class);
        // Its mark, unread, is its own: a copy's would be the live call's.
        ctx.state.clear_setup_cancelled(call_ref, cseq);
        return Turn::Consumed;
    }

    let mut call = build_initial_call(req, src, &ctx.config, &mut turn.ids, now_ms);
    // A call is born only with a record that rebuilds its caller's INVITE
    // (ADR-0052 §11): an INVITE whose record does not is refused from the
    // INVITE itself.
    let Ok(caller) = crate::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite) else {
        let resp = build_unborn_500(&mut turn.ids, req, &ctx.config.minted_final_advertisement);
        if let Ok(resp) = resp {
            let _ = ctx.txn.send_response(resp, reply_to(req, src)).await;
        }
        ctx.metrics.new_calls().reject(Refusal::Unrecoverable, class);
        ctx.state.clear_setup_cancelled(call_ref, cseq);
        return Turn::Shed;
    };

    // The caller CANCELed while this turn waited (marked at ingress): the
    // transaction layer already answered 487. The call is born so the queued
    // `Cancelled` turn ends it with its Cancel CDR; it is neither accepted nor
    // treated (`adm`). A cordoned node refuses the call before its decision;
    // the call is born only to write its record (ADR-0050 §9).
    let cancelled = ctx.state.is_setup_cancelled(call_ref, cseq);
    let cordoned = !cancelled && ctx.readiness.state() == ReadinessState::Cordoned;
    if cancelled {
        ctx.metrics.new_calls().cancel(class);
    } else if cordoned {
        ctx.metrics.new_calls().reject(Refusal::Cordoned, class);
    } else {
        ctx.metrics.new_calls().accept(class);
        // The `adm` published on X-Overload counts non-emergency calls only:
        // the LB's AIMD caps non-emergency traffic alone.
        if class != Class::Emergency {
            ctx.overload.increment_non_emergency_admitted();
        }
    }

    if let Some(ring) = crate::message_ring::Ring::of(&ctx.config) {
        let a_leg = call.a_leg.leg_id.clone();
        call = ring.invite_received(call, &a_leg, req, now_ms);
    }
    // The call is placed on the worker pair the proxy's cookie names; the
    // backlog toward that backup may leave the call with none, for life
    // (ADR-0055 §3).
    let topology = topology_from_cookie(req, &ctx.config.self_ordinal, call_ref);
    let owners = crate::store::envelope::owners(&ctx.relay_owner, topology.as_ref());
    let backed = ctx.state.admit_backup(call_ref, topology.as_ref());
    ctx.state.create(call.clone(), topology);
    // The caller's 100 follows the call's creation write through the call's
    // outbox, so the backup holds the call from before the caller stops
    // retransmitting, not from its decision (ADR-0047, ADR-0055 §3); the
    // transaction layer sends it itself at the §17.2.1 bound.
    if backed {
        ctx.state.flush(&call);
    }
    let (sender, invite) = (ctx.clone(), req.clone());
    let release: super::outbox::Effects = Box::new(move |_| {
        Box::pin(async move {
            let _ = sender.txn.release_trying(&invite).await;
        })
    });
    ctx.outboxes.push(call_ref, ctx.state.hold_target(call_ref), release).await;
    // Born: the call counts as live from here.
    drop(unborn);
    // RFC 3261 §8.1.1.3: a dialog-forming INVITE MUST carry a From tag. The
    // caller's From tag IS the a-leg dialog's remote tag, so admitting a
    // tag-less INVITE would seed an un-probeable a-leg dialog (its in-dialog
    // keepalive OPTIONS could never be built — see `send_request_to_leg`),
    // producing the "OPTIONS to called, not calling" asymmetry that also
    // round-trips through HA hydration. Reject malformed at ingest instead.
    // Created-then-rejected mirrors the decision-reject path so the Terminated
    // invariant reaps the call + propagates the delete.
    // The consult reads the setup-CANCEL mark itself, after its last await;
    // the other arms await nothing and leave it to the drop guard below.
    let (mut consulted, mut recorded) = (false, false);
    let mut handled = if cancelled {
        cancelled_setup(call.clone(), &caller, datagram, now_ms)
    } else if cordoned {
        refused_unavailable(ctx, call.clone(), &caller, now_ms, &mut turn.ids)
    } else if call.a_leg_invite.from_tag.is_empty() {
        crate::initial_invite::reject_call(
            call.clone(),
            &caller,
            400,
            Some("Bad Request - missing From tag".into()),
            None,
            &[],
            &ctx.config.minted_final_advertisement,
            &mut turn.ids,
            now_ms,
            TerminationCause::Admission,
        )
    } else {
        consulted = true;
        // `datagram` is the only place the INVITE as it arrived exists (the
        // call carries a header snapshot, not bytes).
        let setup_cancelled = || ctx.state.is_setup_cancelled(call_ref, cseq);
        let consult = handle_initial_invite(
            call.clone(),
            &caller,
            ctx.decision.as_ref(),
            &ctx.limiter,
            &ctx.config,
            &owners,
            turn,
            &ctx.services,
            datagram,
            &ctx.clock,
            now_ms,
            &setup_cancelled,
        )
        .await;
        match consult {
            Consulted::Applied(result) => *result,
            // The caller CANCELed during the round trip: the txn layer already
            // finalized the a-leg INVITE (200 + 487, the transaction's ONE
            // final, RFC 3261 §17.2.1) and the `Cancelled` event is queued
            // right behind this turn. The decision is recorded, never applied:
            // no b-leg launch, no authored final; the queued `handle-cancel`
            // turn owns the termination (its Cancel CDR keeps the ADR-0022 X2
            // synthesis silent).
            Consulted::Cancelled(decided) => {
                recorded = true;
                ctx.metrics.bump_decision_dropped_cancelled();
                tracing::debug!(
                    %call_ref,
                    "decision recorded, not applied: caller CANCELed during the decision round trip"
                );
                HandlerResult {
                    call: recorded_turn(&call, *decided),
                    effects: HandlerEffects::new(),
                }
            }
        }
    };
    // ── Drop guard ────────────────────────────────────────────────────
    // The caller CANCELed while an arm that consults nothing ran (the
    // malformed-INVITE 400, the cordon's 503): its result is moot, dropped
    // whole. The mark write (ingress run loop) and the reads here and in the
    // consult are not ordered by the per-call lock, so a marking that lands
    // after them reverts to the pre-drop path for that scheduler sliver — the
    // b-leg is launched then CANCELed (§9.1-held until a provisional),
    // indistinguishable from the unavoidable wire race.
    if !cancelled && !consulted && ctx.state.is_setup_cancelled(call_ref, cseq) {
        handled = dropped_on_setup_cancel(&call, handled);
    } else if !cancelled && !recorded && ctx.readiness.left_the_slice() {
        // This node's endpoint left the slice while the decision was in
        // flight, so the decision is not applied: the callee is never dialled
        // for a call its backups took over (ADR-0047). The caller is refused
        // here whether or not a backup answered it too.
        let ids = &mut turn.ids;
        handled = refused_after_leaving_the_slice(ctx, &call, &caller, handled, now_ms, ids);
    }
    let enforced = crate::rules::invariants::enforce(
        &ctx.obligations,
        &call,
        crate::rules::invariants::finalize(handled),
        now_ms,
        crate::rules::invariants::UnansweredCaller::Answer(&ctx.config.minted_final_advertisement),
        &mut turn.notes,
    );
    let mut exec = ActionExecutor { config: &ctx.config, owners: &owners, turn, now_ms };
    Turn::Result(Box::new(close_turn(&ctx.rules, &TurnStart::unborn(), enforced, &mut exec)))
}

/// [`refused_unavailable`] from the pre-handler call ([`discarded_turn`]), so
/// the termination releases what the route admitted.
fn refused_after_leaving_the_slice(
    ctx: &RouterCtx,
    pre: &Call,
    caller: &SipRequest,
    handled: HandlerResult,
    now_ms: i64,
    ids: &mut TurnIds,
) -> HandlerResult {
    refused_unavailable(ctx, discarded_turn(pre, handled.call), caller, now_ms, ids)
}

/// `call`, whose caller's INVITE is `caller`, refused 503 with no
/// Retry-After, as for an unavailable decision (ADR-0022 X3): the record of a
/// call no decision reached.
fn refused_unavailable(
    ctx: &RouterCtx,
    call: Call,
    caller: &SipRequest,
    now_ms: i64,
    ids: &mut TurnIds,
) -> HandlerResult {
    crate::initial_invite::reject_call(
        call,
        caller,
        503,
        Some("Service Unavailable".into()),
        None,
        &[],
        &ctx.config.minted_final_advertisement,
        ids,
        now_ms,
        TerminationCause::Admission,
    )
}

/// The drop-guard result: the pre-handler call — no b-leg, no wire effects.
fn dropped_on_setup_cancel(pre: &Call, handled: HandlerResult) -> HandlerResult {
    HandlerResult { call: discarded_turn(pre, handled.call), effects: HandlerEffects::new() }
}

/// The pre-handler call carrying the decision `decided` holds, recorded and
/// not applied: its decision log, its callback context and the call ext the
/// decision seeded, beside what [`discarded_turn`] keeps. No b-leg, no service
/// cursor: no service ran its `init` on it.
fn recorded_turn(pre: &Call, decided: Call) -> Call {
    let decision_log = decided.decision_log.clone();
    let decision_ordinal = decided.decision_ordinal;
    let callback_context = decided.callback_context.clone();
    let ext = decided.ext.clone();
    let mut call = discarded_turn(pre, decided);
    call.decision_log = decision_log;
    call.decision_ordinal = decision_ordinal;
    call.callback_context = callback_context;
    call.ext = ext;
    call
}

/// The pre-handler call carrying ONLY what a discarded turn already committed
/// *outside* the call. The limiter state `apply_route` admitted rides over so
/// the termination's standard obligation discharge still releases the call
/// (the admit↔release pairing survives the drop), and the trace stamp rides over
/// so a sampled call's later turns (the CANCEL, the 487, the termination that
/// closes its registry entry) still emit into its trace.
fn discarded_turn(pre: &Call, turn: Call) -> Call {
    let mut call = pre.clone();
    call.limiter = turn.limiter;
    call.trace_id = turn.trace_id;
    call.root_span_id = turn.root_span_id;
    call.sampled = turn.sampled;
    call
}
