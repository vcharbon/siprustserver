//! The per-call handler body: runs on the per-call FIFO with the state lock
//! held — reaper verdict gate, the initial-INVITE turn, the incarnation gate,
//! the in-dialog lookup (resident, or materialised as a takeover / on-demand
//! reclaim), the re-offer of an unmatched datagram to the transactions the
//! call seeded, the CANCEL the layer matched nothing for, the rule chain, and
//! the message-cap defense.

use std::sync::Arc;

use call::helpers::Arrival;
use call::{Call, CallModelState, Obligation, TimerType};
use sip_message::{Method, SipMessage};

use super::admit::UnbornCall;
use super::interpret::process_result;
use sip_txn::Reoffer;

use super::materialise::{materialise, reoffer_trigger, Materialised, Origin, Reason};
use super::peer_metrics::record_keepalive_timeout_peer;
use super::reclaim::discharge_as_own;
use super::release::{release_call, ReleaseKind};
use super::resolve::Resolution;
use super::stray_cancel::reject_stray_cancel;
use super::superseded_relay::drop_superseded_relay;
use super::RouterCtx;
use crate::answer_deadline::Screened;
use crate::effects::{HandlerEffects, HandlerResult, QuietTurn};
use crate::limiter::refresh_batch::RefreshAnswered;
use crate::rules::actions::{Owed, Settle};
use crate::rules::lifecycle_delivery::{close_turn, TurnStart};
use crate::rules::{execute_rules, ActionExecutor, RuleCall, RuleContext};
use b2bua_sdk::event::CallEvent;

/// The per-call handler body: check the call out, run the handler, interpret.
/// `unborn` holds a new call admitted at ingress until its call is created;
/// every other exit drops it.
pub(super) async fn process(
    ctx: &Arc<RouterCtx>,
    event: CallEvent,
    mut res: Resolution,
    unborn: Option<UnbornCall>,
) {
    let call_ref = res.call_ref.clone().expect("dispatched events carry a callRef");
    let _guard = ctx.state.lock(&call_ref).await;
    let now_ms = ctx.clock.now_ms();

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
        match super::initial_turn::initial_invite_turn(ctx, &call_ref, &req, src, now_ms, unborn)
            .await
        {
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
        let Some(res) = crate::limiter::call::apply_answer(&ctx.metrics, call, &answer, now_ms)
        else {
            return;
        };
        crate::rules::invariants::enforce(
            &ctx.obligations,
            &before,
            crate::rules::invariants::finalize(res),
            now_ms,
            crate::rules::invariants::UnansweredCaller::Answer(
                &ctx.config.minted_final_advertisement,
            ),
        )
    } else {
        if of_another_incarnation(ctx, &event, &res, |stamp| {
            ctx.state.is_incarnation(&call_ref, stamp) == Some(false)
        }) {
            maybe_reject_orphan(ctx, &event).await;
            return;
        }
        if super::store_fault_gate::in_dialog_store_fault_gate(ctx, &event, &call_ref, now_ms).await
        {
            return;
        }
        let Some(call) = resident_or_materialised(ctx, &call_ref).await else {
            maybe_reject_orphan(ctx, &event).await;
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
            maybe_reject_orphan(ctx, &event).await;
            return;
        }
        // A timer fire reaches the call only while its ledger holds the fired
        // instance, which leaves the ledger as it is delivered.
        let mut call = call;
        if super::timer_screen::screen(&mut call, &event, now_ms).is_err() {
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
        if reoffer_trigger(ctx, &call, &event).await == Reoffer::Matched {
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
            let res = crate::limiter::call::on_refresh_due(&ctx.limiter, call, &call_ref, now_ms);
            crate::rules::invariants::enforce(
                &ctx.obligations,
                &before,
                crate::rules::invariants::finalize(res),
                now_ms,
                crate::rules::invariants::UnansweredCaller::Answer(
                    &ctx.config.minted_final_advertisement,
                ),
            )
        } else {
            // A relay no node can finish is gone before the rules read the
            // call, so its request is relayed afresh.
            let call = drop_superseded_relay(call, &event);
            rule_chain_turn(ctx, call, &event, &res, resolved, &call_ref, now_ms)
        }
    };

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
/// (fatal-error/discharge). Running before the in-dialog lookup means a late
/// verdict for a RELEASED call can never resurrect it from the replica store
/// via on-demand reclaim.
async fn reaper_verdict_gate(
    ctx: &Arc<RouterCtx>,
    event: &CallEvent,
    call_ref: &str,
    now_ms: i64,
) -> Gate {
    if let CallEvent::InternalEvent { topic, outcome, payload, .. } = event {
        if topic == crate::reaper::REAPER_TOPIC {
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

/// The incarnation gate: an event stamped with another incarnation than the
/// call live on its `call_ref` (`differs`) is a straggler of an earlier call
/// there — a timer fire or transaction timeout queued before that call's
/// release, a callout result, a late message on its Via or Contact — and is
/// dropped before any rule reads it; the caller answers a request as one
/// naming no call (481, RFC 3261 §12.2.2). An admit it carries releases its
/// key when owed, as one reaching no call does. An unstamped event passes.
fn of_another_incarnation(
    ctx: &RouterCtx,
    event: &CallEvent,
    res: &Resolution,
    differs: impl FnOnce(&str) -> bool,
) -> bool {
    let Some(stamp) = res.incarnation.as_deref() else { return false };
    if !differs(stamp) {
        return false;
    }
    ctx.metrics.bump_other_incarnation_dropped();
    super::callouts::release_admit_fold_call(&ctx.limiter, event);
    tracing::debug!(
        call_ref = res.call_ref.as_deref().unwrap_or_default(),
        incarnation = stamp,
        kind = event.kind(),
        "event of another call incarnation dropped"
    );
    true
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

/// The branch of a BYE client transaction `event` ends: a final to it, or a
/// transaction timeout (whose branch names it alone).
fn bye_transaction_end(event: &CallEvent) -> Option<&str> {
    match event {
        CallEvent::Sip { message, .. } => match message.as_ref() {
            SipMessage::Response(r) if r.status() >= 200 && r.cseq().method() == Method::Bye => {
                r.top_via().branch()
            }
            _ => None,
        },
        CallEvent::Timeout { branch, .. } => Some(branch.as_str()),
        _ => None,
    }
}

/// Run the synchronous rule chain for one in-dialog event, with the
/// MAX_MESSAGES_PER_CALL cap-defense wrapped around it.
///
/// The count opens at 1 on the initial INVITE (`initial_invite`) and every
/// in-dialog rule-chain event adds one, except a rung of this node's own
/// ladder: a rung repeats retained bytes on this node's clock and is no
/// message of the peer's, so a deaf peer under a lossy path is not a runaway
/// dialog. A repeated inbound 2xx is the peer's message and counts. If the
/// bump crosses `max_messages_per_call` and the handler did not itself
/// terminate the call, append a begin-termination so a runaway dialog
/// (re-INVITE/OPTIONS storm, glare loop, a peer that never stops) is torn down
/// instead of processing unbounded in-dialog events forever — each of which
/// allocates a txn (`set_txn`), a working `Call` clone, and a store body. The
/// async limiter-refresh does not count. The bump rides the turn's own write;
/// on a quiet turn (`QuietTurn`) it rides the next write. Order: bump +
/// capture `cap_exceeded` BEFORE the handler runs, terminate AFTER, so the
/// in-flight event (e.g. relaying this re-INVITE's response) is still serviced
/// before teardown; a turn that trips the cap is the teardown, never quiet.
pub(super) fn rule_chain_turn(
    ctx: &Arc<RouterCtx>,
    mut call: Call,
    event: &CallEvent,
    res: &Resolution,
    resolved: call::helpers::Resolved,
    call_ref: &str,
    now_ms: i64,
) -> HandlerResult {
    // The cap budget is a PER-LIVENESS-INTERVAL rate, not a lifetime total: the
    // keepalive tick RESETS the counter, opening a fresh window. A runaway
    // dialog still lands >cap events inside one window and is torn down; a
    // healthy long call — whose per-interval traffic is its own self-paced
    // probes plus a bounded trickle — never consumes the defense meant for
    // runaway peers. A lifetime reading tore down every hour-long keepalive
    // call around message 200, BYEing a healthy dialog mid-call.
    let bumped = if fired(event, &TimerType::Keepalive) {
        0
    } else if fired(event, &TimerType::Rung) {
        call.message_count.unwrap_or(0)
    } else {
        call.message_count.unwrap_or(0) + 1
    };
    call.message_count = Some(bumped);
    let cap_exceeded = bumped > ctx.config.max_messages_per_call as i64
        && !matches!(call.state, CallModelState::Terminating | CallModelState::Terminated);
    let exec = ActionExecutor {
        config: &ctx.config,
        id_gen: &ctx.id_gen,
        now_ms,
        wire_faults: &ctx.wire_faults,
    };
    // The dialog-level ladders are the framework's (ADR-0032 X4): a rung is
    // repeated here and never reaches a rule; the discharging ACK or PRACK
    // retires its ladder before the rules run and they read the fact; a
    // give-up's timers are scrubbed before the CORE give-up rule decides, and
    // its verdict is settled after (`settle_give_up`: an un-ACKed 2xx ends the
    // session whatever the rules made of it).
    let mut ladder_fx = HandlerEffects::new();
    // A service request's answer, or its deadline, is screened against the
    // call's pending deadlines before anything reads it (ADR-0039): an expired
    // deadline is read as the request's lost answer, and an answer or a
    // deadline no longer awaited reaches no rule.
    let lost_answer;
    let (event, rules_read) = match crate::answer_deadline::screen(&mut call, &mut ladder_fx, event)
    {
        Screened::Expired(lost) => {
            lost_answer = lost;
            (&lost_answer, true)
        }
        Screened::Unawaited => (event, false),
        Screened::Awaited | Screened::Other => (event, true),
    };
    if let Some(obligation) = fired_obligation(event, &TimerType::Rung) {
        let before = call.clone();
        exec.repeat(&mut call, &mut ladder_fx, obligation);
        ladder_fx.quiet = Some(QuietTurn::OwnRung);
        let repeated = HandlerResult { call, effects: ladder_fx };
        return crate::rules::invariants::enforce(
            &ctx.obligations,
            &before,
            crate::rules::invariants::finalize(repeated),
            now_ms,
            crate::rules::invariants::UnansweredCaller::Answer(
                &ctx.config.minted_final_advertisement,
            ),
        );
    }
    let start = TurnStart::of(&call);
    // A reliable provisional is the engine's before it is any rule's (RFC
    // 3262): recorded with its early dialog and offer, or — a repeat, or out
    // of `RSeq` order — dropped unread, its turn a write of nothing else.
    let arrival = exec.take_reliable_provisional(&mut call, event, &res.source_leg_id);
    let screened = matches!(arrival, Some(Arrival::Repeat | Arrival::OutOfOrder));
    let rules_read = rules_read && !screened;
    let discharged = exec.discharge(&mut call, &mut ladder_fx, event, &res.source_leg_id);
    if let CallEvent::Sip { message, .. } = event {
        if let SipMessage::Response(resp) = message.as_ref() {
            let leg = res.source_leg_id.as_str();
            super::response_facts::record(&exec, &mut call, &mut ladder_fx, leg, resp, screened);
        }
    }
    // The end of a BYE that released a retired dialog, its final or its
    // timeout, settles that release (§15.1.1) before the rules read it.
    if let Some(branch) = bye_transaction_end(event) {
        call = call::helpers::settle_straggler_release(call, branch);
    }
    // The transaction layer answered an out-of-dialog CANCEL itself — 200 to
    // the CANCEL, 487 to the initial INVITE (RFC 3261 §9.2): the record
    // carries that transaction's one final before the rules read it, over a
    // 2xx this stack built too late for the completed transaction to send.
    if let CallEvent::Cancelled { in_dialog: false, .. } = event {
        call = call::helpers::record_cancelled_invite(call, &res.source_leg_id);
    }
    if let Some(obligation) = fired_obligation(event, &TimerType::RepeatGiveUp) {
        ctx.metrics.record_repeat_give_up(obligation.kind());
        exec.give_up(&mut call, &mut ladder_fx, obligation);
    }
    // The record names the message before the rules read it, so what the
    // turn sends follows what it received.
    if let Some(ring) = crate::message_ring::Ring::of(&ctx.config) {
        let dialog = resolved.dialog.as_ref();
        let repeat = arrival == Some(Arrival::Repeat);
        call = ring.received(
            call,
            &res.source_leg_id,
            event,
            repeat,
            discharged.as_ref(),
            dialog,
            now_ms,
        );
    }
    // An INVITE (offerless included) or an UPDATE offer the leg's peer sends
    // opens an exchange on that leg, counted once as it is received, before
    // the rules read it.
    if let CallEvent::Sip { message, .. } = event {
        if let SipMessage::Request(req) = message.as_ref() {
            let source = resolved.dialog.as_ref();
            crate::rules::relay::note_request(&mut call, &res.source_leg_id, req, source);
        }
    }
    // A decision folding back while its consult is awaited records the routes
    // admission refused before it, then its mark, whichever rule claims it.
    if rules_read {
        crate::attempts::record_refused_routes(&mut call, &mut ladder_fx, event);
        call = crate::decision_log::fold_decided(call, event, now_ms);
    }
    // An admit sent off the call's turn is applied before the rules read its
    // event (an unawaited fold's route is not the call's); one of another key
    // (an earlier call under this call_ref) releases that key when owed.
    let fx = &mut ladder_fx;
    if let Some(report) = crate::limiter::report::apply_to_call(&mut call, event, rules_read, fx) {
        if crate::trace::sampled(&call) {
            let what = format!("{:?} -> {:?}", report.entries, report.outcome);
            crate::trace::emit::limiter(&call, now_ms, "admit", &what);
        }
    }
    let rule_ctx = RuleContext {
        call: RuleCall::new(&call),
        call_ref,
        event,
        source_leg_id: &res.source_leg_id,
        direction: res.direction,
        now_ms,
        config: &ctx.config,
        discharged: discharged.as_ref(),
        resolved: &resolved,
    };
    let mut result = if rules_read {
        execute_rules(&ctx.rules, &call, &rule_ctx, &exec, &ctx.obligations)
    } else {
        HandlerResult { call: call.clone(), effects: HandlerEffects::new() }
    };
    // The final to the INVITE that dialled the leg is its outcome, recorded
    // after the rules matched the leg as the response found it: a rule that
    // acted on it already recorded it, the first final standing (§17.1.1.2).
    if let CallEvent::Sip { message, .. } = event {
        if let SipMessage::Response(resp) = message.as_ref() {
            if resp.status() >= 200 && resp.cseq().method() == Method::Invite {
                result.call = call::helpers::record_dialled_invite_final(
                    result.call,
                    &res.source_leg_id,
                    i64::from(resp.cseq().seq()),
                    resp.status(),
                );
            }
        }
    }
    super::quiet_turn::name_candidate(&mut result, &call, event, discharged.as_ref());
    // The ladder's own effects (its cancels) precede the rule's.
    ladder_fx.extend(std::mem::take(&mut result.effects));
    result.effects = ladder_fx;
    if let Some(obligation) = fired_obligation(event, &TimerType::RepeatGiveUp) {
        let before = result.call.clone();
        let settled = exec.settle_give_up(result, obligation, &rule_ctx);
        result = crate::rules::invariants::enforce(
            &ctx.obligations,
            &before,
            crate::rules::invariants::finalize(settled),
            now_ms,
            crate::rules::invariants::UnansweredCaller::Answer(
                &ctx.config.minted_final_advertisement,
            ),
        );
    }
    if cap_exceeded
        && !matches!(result.call.state, CallModelState::Terminating | CallModelState::Terminated)
    {
        result = super::message_cap::tear_down_runaway(ctx, &exec, result, &rule_ctx);
    }
    // A reliable provisional shown to no one, or whose INVITE, leg or dialog
    // the turn ended, is PRACKed by the engine whatever the rules made of it
    // (RFC 3262 §4).
    exec.settle_owed_pracks(&mut result.call, &mut result.effects, Owed::Call, Settle::TurnEnd);
    // The turn's lifecycle closes over what every layer above made of it.
    close_turn(&ctx.rules, &start, result, &exec)
}

/// Whether `event` is a fire of a timer of type `timer_type`.
fn fired(event: &CallEvent, timer_type: &TimerType) -> bool {
    matches!(event, CallEvent::Timer { key, .. } if &key.timer_type == timer_type)
}

/// The obligation of a fired `timer_type` ladder timer.
fn fired_obligation<'a>(event: &'a CallEvent, timer_type: &TimerType) -> Option<&'a Obligation> {
    match event {
        CallEvent::Timer { key, .. } if &key.timer_type == timer_type => key.obligation_of(),
        _ => None,
    }
}

/// A request for a vanished call draws the 200 of a PRACK its released call
/// still answers ([`super::late_prack::answer`]), else the answer a request
/// naming no call is owed ([`super::unroutable::refusal`]); an ACK or a
/// response draws nothing.
async fn maybe_reject_orphan(ctx: &RouterCtx, event: &CallEvent) {
    if let CallEvent::Sip { message, src, .. } = event {
        if let SipMessage::Request(req) = message.as_ref() {
            let answer = super::late_prack::answer(ctx, req)
                .or_else(|| super::unroutable::refusal(ctx, req));
            if let Some(answer) = answer {
                let _ = ctx.txn.send_response(answer, *src).await;
            }
        }
    }
}
