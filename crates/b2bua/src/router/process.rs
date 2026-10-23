//! The per-call handler body: runs on the per-call FIFO with the state lock
//! held — reaper verdict gate, the initial-INVITE turn, the incarnation gate,
//! the in-dialog lookup (resident, or materialised as a takeover / on-demand
//! reclaim), the re-offer of an unmatched datagram to the transactions the
//! call seeded, the CANCEL the layer matched nothing for, then the rule chain
//! ([`super::rule_chain`]).

use std::sync::Arc;

use call::{Call, TimerType};
use sip_message::SipMessage;

use super::admit::UnbornCall;
use super::incarnation_gate::of_another_incarnation;
use super::interpret::process_result;
use sip_txn::Reoffer;

use super::materialise::{materialise, reoffer_trigger, Materialised, Origin, Reason};
use super::peer_metrics::record_keepalive_timeout_peer;
use super::reclaim::discharge_as_own;
use super::release::{release_call, ReleaseKind};
use super::resolve::Resolution;
use super::stray_cancel::reject_stray_cancel;
use super::unrecoverable::discharge_after_turn;
use super::unroutable::answer_vanished;
use super::RouterCtx;
use crate::limiter::refresh_batch::RefreshAnswered;
use crate::rule_chain::{enforced, fired, rule_chain_turn, TurnInput};
use crate::turn_scope::TurnScope;
use b2bua_sdk::event::CallEvent;

/// The per-call handler body: check the call out, run the handler, interpret.
/// `matched_client_txn`: a response `event` carries answered a client
/// transaction the transaction layer holds. `unborn` holds a new call
/// admitted at ingress until its call is created; every other exit drops it.
pub(super) async fn process(
    ctx: &Arc<RouterCtx>,
    event: CallEvent,
    matched_client_txn: bool,
    mut res: Resolution,
    unborn: Option<UnbornCall>,
) {
    let Some(call_ref) = res.call_ref.clone() else {
        // The dispatcher keys every event it runs by its callRef.
        tracing::error!(node = observe::node(), "a dispatched event carries no callRef: dropped");
        return;
    };
    let _guard = ctx.state.lock(&call_ref).await;
    let now_ms = ctx.clock.now_ms();
    // The turn opens before its gates: their refusals draw identifiers too.
    let mut turn = TurnScope::of(ctx.id_gen.turn()).with_faults(ctx.wire_faults.snapshot());

    match reaper_verdict_gate(ctx, &event, &call_ref, now_ms).await {
        Gate::Consumed => return,
        Gate::Shed => {
            drop(_guard);
            release_call(ctx, &call_ref, ReleaseKind::Orphan).await;
            return;
        }
        Gate::Pass => {}
    }

    let result = if res.initial_invite {
        let (req, src) = match &event {
            CallEvent::Sip { message, src, .. } => match message.as_ref() {
                SipMessage::Request(r) => (r.clone(), *src),
                _ => return,
            },
            _ => return,
        };
        let initial = super::initial_turn::initial_invite_turn(
            ctx, &call_ref, &req, src, now_ms, unborn, &mut turn,
        );
        match initial.await {
            super::initial_turn::Turn::Consumed => return,
            // A final went on the wire with no call here, but this dispatch created a
            // per-call queue (one `bump_creation`) + lock entry for a brand-new
            // call_ref and nothing will ever emit `RemoveCall`. Release through
            // the one teardown executor (`ReleaseKind::Orphan`: no store
            // mutation, so no spurious reverse-propagated delete) so the worker
            // exits and `removals` balances `creations`. Drop our guard first.
            super::initial_turn::Turn::Shed => {
                drop(_guard);
                release_call(ctx, &call_ref, ReleaseKind::Orphan).await;
                return;
            }
            super::initial_turn::Turn::Result(r) => *r,
        }
    } else if let Some(answer) = RefreshAnswered::of(&event) {
        // A refresh answer applies to the resident call and never
        // materialises one: an ended call (or one that left this worker)
        // takes no answer, and the dispatch's per-call ephemera go.
        let Some(call) = ctx.state.peek(&call_ref) else {
            ctx.metrics.limiter().count_refresh_discarded(crate::metrics::RefreshDiscard::CallGone);
            drop(_guard);
            release_call(ctx, &call_ref, ReleaseKind::Orphan).await;
            return;
        };
        let before = call.clone();
        let Some(res) =
            crate::limiter::refresh_turn::apply_answer(&ctx.metrics, call, &answer, now_ms)
        else {
            return;
        };
        enforced(&ctx.framework(), &before, res, now_ms, &mut turn.notes)
    } else {
        if of_another_incarnation(ctx, &event, &res, |stamp| {
            ctx.state.is_incarnation(&call_ref, stamp) == Some(false)
        }) {
            answer_vanished(ctx, &event, &mut turn.ids).await;
            return;
        }
        if super::store_fault_gate::in_dialog_store_fault_gate(ctx, &event, &call_ref, now_ms).await
        {
            return;
        }
        let Some(call) = resident_or_materialised(ctx, &call_ref).await else {
            answer_vanished(ctx, &event, &mut turn.ids).await;
            // An admit report for a vanished call (a route fold, a service's
            // admit result): no call is left to own the set its admit may
            // have counted.
            super::callouts::release_admit_fold_call(&ctx.limiter, &event);
            // This event was dispatched into a fresh per-call queue (one
            // `bump_creation`) and took the per-call lock, but resolved to NO
            // live call — nothing will ever emit `RemoveCall`, and a per-call
            // dispatch worker exits ONLY on poison, so the queue, its idle
            // task, the unmatched creation, and the lock entry would all leak
            // permanently (a mass-orphan failover turns that into an
            // `active_calls`/`store_locks` ratchet that never drains). Release
            // through the one teardown executor (`ReleaseKind::Orphan` — no
            // store mutation, so no spurious reverse-propagated delete). Drop
            // our guard first so the poisoned worker never contends on this
            // call_ref's (now removed) lock.
            drop(_guard);
            release_call(ctx, &call_ref, ReleaseKind::Orphan).await;
            return;
        };
        res.bind_indexed_leg(&call, &event);
        // A copy this event materialised is checked as a resident one was.
        if of_another_incarnation(ctx, &event, &res, |stamp| call.incarnation() != stamp) {
            answer_vanished(ctx, &event, &mut turn.ids).await;
            return;
        }
        // A timer fire reaches the call only while its ledger holds the fired
        // instance, which leaves the ledger as it is delivered.
        let mut call = call;
        let screened =
            crate::rule_chain::timer_screen::screen(&mut call, &event, now_ms, &mut turn.notes);
        super::notes::drain(ctx, &call_ref, &mut turn);
        if screened.is_err() {
            return;
        }
        // Traced call: the message as it arrived, raw (ADR-0026), before any
        // answer to it — the re-offer's or the stray-CANCEL 481 — so the
        // datagram that triggered a takeover is in the trace. `image()` is the
        // received datagram itself, so a lenient-parser normalization — a
        // folded header, an odd-cased name, a rewritten URI — stays visible in
        // the very artifact that exists to diagnose it; re-serializing the
        // parse would hide it. Guarded, and it borrows: no copy either way.
        if let CallEvent::Sip { message, src, .. } = &event {
            if crate::trace::sampled(&call) {
                crate::trace::emit::sip_in(&call, now_ms, *src, message.image());
            }
        }
        // The message is resolved to the dialog it names, or refused, before
        // any of the machinery below reads it (RFC 3261 §12.2.2, ADR-0041).
        let resolved = match super::dialog_gate::admit(ctx, &call, &event, &res).await {
            super::dialog_gate::Gate::Pass(resolved) => resolved,
            super::dialog_gate::Gate::Answered { answer, in_order } => {
                let leg = res.source_leg_id.as_str();
                let call = match &in_order {
                    Some(r) => super::dialog_gate::note_in_order(call, leg, r, &event),
                    None => call,
                };
                let taken = in_order.is_some();
                record_refusal(ctx, call, leg, &event, answer.as_deref(), taken, now_ms);
                return;
            }
            super::dialog_gate::Gate::Dropped => return,
        };
        let call = super::dialog_gate::note_in_order(call, &res.source_leg_id, &resolved, &event);
        // The layer emitted this datagram with no transaction to match it; a
        // materialisation — this turn's or an earlier one's, whose seeds went in
        // after the datagram was emitted — may hold one now. Re-offered, a match
        // is the layer's to answer and re-emit; this turn ends.
        if reoffer_trigger(ctx, &call, &event, matched_client_txn).await == Reoffer::Matched {
            return;
        }
        // A CANCEL reaches the router only when the transaction layer matched
        // no INVITE for it; the re-offer above was the seeded INVITE's chance.
        // Otherwise RFC 3261 §9.2: 481, and no effect on the call. The rules
        // never see a CANCEL request.
        let own_tag = call::helpers::find_leg(&call, &res.source_leg_id)
            .and_then(call::helpers::leg_own_tag)
            .map(str::to_string);
        if let Some(answer) =
            reject_stray_cancel(ctx, &call, &res.source_leg_id, own_tag.as_deref(), &event).await
        {
            record_refusal(ctx, call, &res.source_leg_id, &event, Some(&answer), false, now_ms);
            return;
        }
        // The limiter-refresh timer marks the call due on the worker's refresh
        // batch outside the rule chain; the call never waits on the limiter.
        if fired(&event, &TimerType::LimiterRefresh) {
            let before = call.clone();
            let limiter = &ctx.limiter;
            let res =
                crate::limiter::refresh_turn::on_refresh_due(limiter, call, &call_ref, now_ms);
            enforced(&ctx.framework(), &before, res, now_ms, &mut turn.notes)
        } else {
            let input = TurnInput {
                event: &event,
                call_ref: &call_ref,
                source_leg_id: &res.source_leg_id,
                direction: res.direction,
                resolved,
                matched_client_txn,
                now_ms,
            };
            rule_chain_turn(&ctx.framework(), call, input, &mut turn)
        }
    };
    let unrecoverable = turn.notes.unrecoverable();
    super::notes::drain(ctx, &call_ref, &mut turn);
    if unrecoverable {
        let ids = &mut turn.ids;
        if !discharge_after_turn(ctx, &call_ref, &event, now_ms, ids).await {
            drop(_guard);
            release_call(ctx, &call_ref, ReleaseKind::Orphan).await;
        }
        return;
    }

    record_keepalive_timeout_peer(ctx, &event, &result.call);

    let turn = super::turn_label::EventSummary { call_ref: &call_ref, event: &event };
    let received = match &event {
        CallEvent::Sip { message, .. } => match message.as_ref() {
            SipMessage::Request(r) => Some(r),
            SipMessage::Response(_) => None,
        },
        _ => None,
    };
    process_result(ctx, &call_ref, result, now_ms, &turn, received).await;
}

/// How a pre-handler gate resolved the event.
enum Gate {
    /// Falls through to the normal handler path.
    Pass,
    /// Fully consumed — return, keeping the per-call ephemera (the call, or a
    /// retransmit whose call exists).
    Consumed,
    /// Consumed AND the dispatch's fresh per-call queue/lock must be torn down
    /// via `ReleaseKind::Orphan` (no call was ever, or will ever be, resident).
    Shed,
}

/// The call-reaper verdict gate (ADR-0020 X5/X6) — runs BEFORE any
/// materialisation, plus the last-touched liveness stamp. A verdict is
/// check-then-act made safe: it applies only if the call's last-touched stamp
/// still matches what the sweep observed (stale) or the call is still resident
/// (fatal-error/discharge, an operator's release ADR-0050 §10). Running before
/// the in-dialog lookup means a late verdict for a RELEASED call can never
/// resurrect it from the replica store via on-demand reclaim.
async fn reaper_verdict_gate(
    ctx: &Arc<RouterCtx>,
    event: &CallEvent,
    call_ref: &str,
    now_ms: i64,
) -> Gate {
    if let CallEvent::InternalEvent { topic, outcome, payload, .. } = event {
        if topic == crate::reaper::REAPER_TOPIC || topic == crate::release_all::TOPIC {
            let watermark = payload.get("watermark").and_then(|v| v.as_i64());
            let current = ctx.state.last_touched(call_ref);
            if !crate::reaper::verdict_confirmed(outcome, watermark, current) {
                if current.is_none() && ctx.state.peek(call_ref).is_none() {
                    // The call is gone — but this verdict's dispatch may have
                    // spun up a fresh per-call queue (+ lock entry). Tear the
                    // ephemera down via the orphan path so nothing ratchets.
                    return Gate::Shed;
                }
                return Gate::Consumed;
            }
            if outcome == crate::reaper::OUTCOME_DISCHARGE {
                // Strike-2: the rules path itself failed. Force the last
                // persisted snapshot terminal and run it through the ORDINARY
                // finalize → enforce → process_result — the ObligationSet
                // discharges the CDR + limiter holds, RemoveCall rides
                // release_call(Terminated), the delete propagates (X6).
                if ctx.state.peek(call_ref).is_none() {
                    return Gate::Consumed;
                }
                ctx.metrics.bump_reaper_discharged();
                discharge_as_own(ctx, call_ref, now_ms).await;
                return Gate::Consumed;
            }
            // A confirmed stale / fatal-error verdict falls through to the
            // normal rules (`reaper-stale` / `reaper-fatal-error`). It does
            // NOT refresh the stamp — an ineffective verdict must keep the
            // call stale so the sweep escalates instead of waiting idle_max.
        }
    } else if matches!(event, CallEvent::Sip { .. }) {
        // The last-touched stamp (ADR-0020 X4): liveness derives from **real
        // SIP traffic only** — a received message here, or a turn that sent
        // SIP out (stamped in `process_result` after the outbound effects).
        // Self-generated turns that touch no wire (`LimiterRefresh`, internal
        // events, timer fires whose rules emit nothing) deliberately do NOT
        // stamp — the call must not vouch for itself, or a crash-orphaned call
        // refreshing its limiter holds would keep itself reaper-"fresh" while
        // SIP-dead. A wedged FIFO never reaches this line — its stamp freezes,
        // which IS the staleness signal the sweep reads.
        ctx.state.touch(call_ref, now_ms);
    }
    Gate::Pass
}

/// The live call for an in-dialog event: resident, or materialised as an
/// acting-backup takeover (`bak:`), or — for a ref this node is primary for —
/// as an on-demand reclaim (`pri:`), the in-dialog request that races the bulk
/// sweep on a rebooted primary. `None` = a genuine orphan, or a deferred
/// terminal this call just discharged through the reclaim funnel (CDR written,
/// delete propagated); the request then draws the orphan 481 (ADR-0020 X3).
async fn resident_or_materialised(ctx: &Arc<RouterCtx>, call_ref: &str) -> Option<Call> {
    let first = materialise(ctx, call_ref, Origin::Takeover).await;
    let disposition = match first {
        Materialised::Refused(Reason::NotBackupRole) => {
            materialise(ctx, call_ref, Origin::Reclaim(None)).await
        }
        other => other,
    };
    match disposition {
        Materialised::Served { call, .. } | Materialised::Resident(call) => Some(call),
        Materialised::Refused(_) => None,
    }
}

/// A request the router answered on a live call's behalf is still a message
/// of the call's: the ring records it and the answer as a turn of their own,
/// and the record lands — also without a ring when the answer `changed` the
/// record (a request taken in order sets its dialog's last CSeq).
fn record_refusal(
    ctx: &RouterCtx,
    call: Call,
    leg_id: &str,
    event: &CallEvent,
    answer: Option<&sip_message::SipResponse>,
    changed: bool,
    now_ms: i64,
) {
    let ring = crate::message_ring::Ring::of(&ctx.config);
    let req = match event {
        CallEvent::Sip { message, .. } => match message.as_ref() {
            SipMessage::Request(req) => Some(req),
            SipMessage::Response(_) => None,
        },
        _ => None,
    };
    let call = match (ring, req) {
        (Some(ring), Some(req)) => {
            call::helpers::seal_turn(ring.refused(call, leg_id, req, answer, now_ms))
        }
        _ if changed => call,
        _ => return,
    };
    ctx.state.update(call);
}
