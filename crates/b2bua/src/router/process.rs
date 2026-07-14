//! The per-call handler body: runs on the per-call FIFO with the state lock
//! held — reaper verdict gate, initial-INVITE admission, in-dialog hydration
//! (including takeover / on-demand reclaim), the rule chain, and the
//! message-cap defense.

use std::net::SocketAddr;
use std::sync::Arc;

use call::{Call, CallModelState, LegState, TimerEntry, TimerType};
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::message_helpers::is_emergency_request;
use sip_message::SipMessage;

use super::interpret::process_result;
use super::peer_metrics::{classify_b2bua_peer, keepalive_timeout_peer};
use super::reclaim::discharge_as_own;
use super::release::{release_call, ReleaseKind};
use super::resolve::Resolution;
use super::responses::{build_stateless_overload_503, build_store_fault_500};
use super::restore_hygiene::sanitize_restored_timers;
use super::RouterCtx;
use crate::effects::{CriticalStateEffect, HandlerEffects, HandlerResult};
use crate::event::CallEvent;
use crate::initial_invite::{build_initial_call, handle_initial_invite};
use crate::rules::model::RuleAction;
use crate::rules::{execute_rules, ActionExecutor, RuleCall, RuleContext};
use crate::store::StoreFaultPoint;

/// The per-call handler body: check the call out, run the handler, interpret.
pub(super) async fn process(ctx: &Arc<RouterCtx>, event: CallEvent, res: Resolution) {
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
            CallEvent::Sip { message, src } => match message.as_ref() {
                SipMessage::Request(r) => (r.clone(), *src),
                _ => return,
            },
            _ => return,
        };
        match initial_invite_turn(ctx, &call_ref, &req, src, now_ms).await {
            Turn::Consumed => return,
            // A stateless shed replied on the wire, but this dispatch created a
            // per-call queue (one `bump_creation`) + lock entry for a brand-new
            // call_ref and nothing will ever emit `RemoveCall`. Release through
            // the one teardown executor (`ReleaseKind::Orphan`: no store
            // mutation, so no spurious reverse-propagated delete) so the worker
            // exits and `removals` balances `creations`. Drop our guard first.
            Turn::Shed => {
                drop(_guard);
                release_call(ctx, &call_ref, ReleaseKind::Orphan).await;
                return;
            }
            Turn::Result(r) => r,
        }
    } else {
        if in_dialog_store_fault_gate(ctx, &event, &call_ref, now_ms).await {
            return;
        }
        let Some(call) = hydrate_or_reclaim(ctx, &call_ref).await else {
            maybe_reject_orphan(ctx, &event).await;
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
        // The limiter-refresh timer is async (an HTTP call to migrate holds), so
        // it is handled outside the synchronous rule chain — like initial-INVITE.
        if matches!(
            &event,
            CallEvent::Timer { timer_type: TimerType::LimiterRefresh, .. }
        ) {
            let before = call.clone();
            let res = handle_limiter_refresh(ctx, call, now_ms).await;
            crate::rules::invariants::enforce(&ctx.obligations, &before, crate::rules::invariants::finalize(res), now_ms, true)
        } else {
            rule_chain_turn(ctx, call, &event, &res, &call_ref, now_ms)
        }
    };

    record_keepalive_timeout_peer(ctx, &event, &result.call);

    process_result(ctx, &call_ref, result, now_ms).await;
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

/// Outcome of the initial-INVITE admission ladder.
enum Turn {
    /// Fully handled (a retransmit for an existing call) — return as-is.
    Consumed,
    /// A stateless reject replied on the wire; the caller tears down the
    /// per-call ephemera this dispatch created (`ReleaseKind::Orphan`).
    Shed,
    /// Admitted (or created-then-rejected): the handler result to interpret.
    Result(HandlerResult),
}

/// The call-reaper verdict gate (ADR-0020 X5/X6) — runs BEFORE any hydration,
/// plus the last-touched liveness stamp. A verdict is check-then-act made safe:
/// it applies only if the call's last-touched stamp still matches what the
/// sweep observed (stale) or the call is still resident (fatal-error/discharge).
/// Running before the in-dialog hydrate means a late verdict for a RELEASED
/// call can never resurrect it from the replica store via on-demand reclaim.
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

/// The initial-INVITE admission ladder: store-fault probe → retransmit guard →
/// Tier-3 admission gate → build + rule the new call.
async fn initial_invite_turn(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    req: &sip_message::SipRequest,
    src: SocketAddr,
    now_ms: i64,
) -> Turn {
    // ── Live-path store-fault probe (ADR-0023): initial INVITE ──────────────
    // The dialog-existence lookup (the `peek` retransmit guard below) is the
    // store read this INVITE depends on; a faulted store cannot answer "does
    // this dialog already exist", so the probe fires BEFORE the peek (its
    // answer is untrustworthy under a fault). Fail CLOSED: a final **500
    // Server Internal Error** through the INVITE server txn — superseding the
    // auto-100, composing with the ADR-0022 no-100-then-silence guarantee —
    // and NO call state is born.
    if ctx.store_faults.check(StoreFaultPoint::LiveInitialInvite).is_err() {
        let resp = build_store_fault_500(&ctx.id_gen, req);
        let _ = ctx.txn.send_response(resp, src).await;
        ctx.metrics.bump_store_fault_rejected();
        return Turn::Shed;
    }

    if ctx.state.peek(call_ref).is_some() {
        return Turn::Consumed; // retransmitted INVITE for an existing call — ignore
    }

    // ── Tier-3 admission gate. Only an *initial* INVITE reaches here;
    // re-INVITEs (To-tag present) and non-INVITE in-dialog requests take the
    // in-dialog branch and are never gated.
    //
    // sip-txn has already created the INVITE server txn and auto-sent
    // 100 Trying before emitting this Message (the ADR-0007 layering
    // deferral), so the reject is sent *through that server txn*
    // (`send_response`, which supersedes the cached 100 and drives the txn →
    // Completed with proper retransmission + ACK absorption) rather than as a
    // wire-raw datagram. It is still **stateless at the call layer** — no
    // `build_initial_call`/`create`, so no dialog, CDR, limiter hold, or
    // replicated state is ever born.
    let is_emergency = is_emergency_request(req);
    let decision = ctx.overload.should_admit(is_emergency);
    if !decision.admit {
        let resp = build_stateless_overload_503(&ctx.id_gen, req, decision.retry_after_sec);
        let _ = ctx.txn.send_response(resp, src).await;
        // The reject is observable via `b2bua_overload_rejected_total`; the
        // `reason`/`retry_after_sec` are carried on the 503 itself (Reason +
        // Retry-After) for the caller and any wire trace.
        ctx.metrics.bump_overload_rejected();
        return Turn::Shed;
    }
    // Counter published on X-Overload (`adm`). Emergency admits are NOT counted
    // on `adm` — the LB's AIMD caps non-emergency traffic only — but ARE
    // tallied on their own `b2bua_emergency_admitted_total` counter so the
    // emergency-admit branch is observable (it would otherwise be uncounted).
    if is_emergency {
        ctx.overload.increment_emergency_admitted();
    } else {
        ctx.overload.increment_non_emergency_admitted();
    }

    let call = build_initial_call(req, src, &ctx.config, now_ms);
    ctx.state.create(call.clone());
    // RFC 3261 §8.1.1.3: a dialog-forming INVITE MUST carry a From tag. The
    // caller's From tag IS the a-leg dialog's remote tag, so admitting a
    // tag-less INVITE would seed an un-probeable a-leg dialog (its in-dialog
    // keepalive OPTIONS could never be built — see `send_request_to_leg`),
    // producing the "OPTIONS to called, not calling" asymmetry that also
    // round-trips through HA hydration. Reject malformed at ingest instead.
    // Created-then-rejected mirrors the decision-reject path so the Terminated
    // invariant reaps the call + propagates the delete.
    let result = if call.a_leg.from_tag.is_empty() {
        let a_invite = crate::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let rejected = crate::initial_invite::reject_call(
            call.clone(),
            &a_invite,
            400,
            Some("Bad Request - missing From tag".into()),
            None,
            &[],
            &ctx.id_gen,
            now_ms,
        );
        crate::rules::invariants::enforce(&ctx.obligations, &call, crate::rules::invariants::finalize(rejected), now_ms, true)
    } else {
        let handled =
            handle_initial_invite(call.clone(), ctx.decision.as_ref(), ctx.limiter.as_ref(), &ctx.config, &ctx.id_gen, &ctx.services, now_ms).await;
        crate::rules::invariants::enforce(&ctx.obligations, &call, crate::rules::invariants::finalize(handled), now_ms, true)
    };
    Turn::Result(result)
}

/// Live-path store-fault probes for in-dialog events (ADR-0023), with the
/// defined degraded-mode semantics. Returns `true` when the event was consumed
/// by a fault path:
///   - in-dialog SIP *request* (BYE, re-INVITE, …): fail CLOSED — 500 through
///     the server txn, call + state untouched (deliberately distinct from the
///     481 lookup-MISS: the call may well exist, the store just cannot say). A
///     retry after recovery proceeds normally. ACK is never answered (RFC 3261
///     §17) — dropped, as the orphan path drops it.
///   - Keepalive audit timer: fail OPEN — skip this probe cycle but RE-ARM the
///     timer so liveness detection resumes next interval; a store fault alone
///     must never tear down an established call (protected-calls invariant,
///     docs/testing/ha-acceptance.md).
///   - everything else (responses, CANCEL/timeout/internal events) is
///     deliberately un-probed: those paths owe no store-derived answer, and
///     absorbing e.g. a keepalive OPTIONS-200 here would convert a store fault
///     into a KeepaliveTimeout teardown of a healthy call.
async fn in_dialog_store_fault_gate(
    ctx: &Arc<RouterCtx>,
    event: &CallEvent,
    call_ref: &str,
    now_ms: i64,
) -> bool {
    if let CallEvent::Sip { message, src } = event {
        if let SipMessage::Request(req) = message.as_ref() {
            if ctx.store_faults.check(StoreFaultPoint::LiveInDialog).is_err() {
                if req.method != "ACK" {
                    let resp = generate_response(
                        req,
                        500,
                        "Server Internal Error",
                        &GenerateResponseOpts::default(),
                    );
                    let _ = ctx.txn.send_response(resp, *src).await;
                }
                ctx.metrics.bump_store_fault_rejected();
                return true;
            }
        }
    }
    if let CallEvent::Timer { timer_type: TimerType::Keepalive, .. } = event {
        if ctx.store_faults.check(StoreFaultPoint::LiveAudit).is_err() {
            ctx.metrics.bump_store_fault_audit_skipped();
            // Re-arm at the config cadence — the same interval the `keepalive`
            // rule re-arms with, read from config because the call body is what
            // we could not fetch. Runtime driver only: the serialized
            // `call.timers` intent stays untouched (the call is untouched),
            // which is safe — a later HA restore sanitizes past-due entries.
            let entry = TimerEntry {
                id: TimerType::Keepalive.timer_id(None),
                timer_type: TimerType::Keepalive,
                fire_at: now_ms + ctx.config.keepalive_interval_sec * 1000,
                leg_id: None,
            };
            ctx.timers.schedule(entry, call_ref.to_string()).await;
            return true;
        }
    }
    false
}

/// Hydrate the call for an in-dialog event: the in-memory map, falling back to
/// the acting-backup takeover read-path (S10b), then the rebooted-primary
/// on-demand reclaim. `None` = a genuine orphan (no replica anywhere).
async fn hydrate_or_reclaim(ctx: &Arc<RouterCtx>, call_ref: &str) -> Option<Call> {
    match ctx.state.hydrate_from_replica(call_ref).await {
        Some((c, fresh, skew_offset_ms)) => {
            // Failover timer re-arm: per-call timers (keepalive, global
            // duration, …) live in this node's in-memory `TimerService`, NOT in
            // the replicated call state — so a call freshly materialized from a
            // backup arrives with no live timers on THIS node. Re-arm its
            // serialized timer intents (`call.timers`, which IS replicated)
            // into the local driver, exactly once, on the hydration that
            // created it. Without this the hydrated call has no keepalive (a
            // dead peer is never probed) and no duration cap (never reaped) →
            // `b2bua_active_calls` leaks on the takeover node. `restore`
            // past-due entries fire immediately (the keepalive then re-arms
            // itself on the next interval via the `keepalive` rule); re-arming
            // is idempotent — any subsequent rule-emitted `ScheduleTimer` for
            // the same id supersedes it via the driver's epoch bump. Skipped
            // for `fresh == false` (the call was already resident and its
            // timers are already live) to avoid double-arm.
            if fresh {
                // Mark this as a live acting-backup takeover copy (ADR-0014)
                // and ARM the self-release notice: the txn layer will send a
                // `CallQuiesced` once the transaction(s) we serve for this call
                // all reach a terminal state, at which point the router sheds
                // the live copy (keeping the `bak:` replica).
                ctx.state.mark_takeover(call_ref);
                // Restore-hygiene seam (clock-skew hardening): re-anchor the
                // failed-over timers by the persisted receive-time skew offset,
                // drop the stale in-flight `KeepaliveTimeout` (the OPTIONS it
                // guarded died with the crashed primary), and apply the
                // deep-past-due keepalive floor — so no immediate OPTIONS races
                // the failed-over re-INVITE. No cohort here (single call), so
                // no smoothing.
                let mut takeover_timers = c.timers.clone();
                sanitize_restored_timers(
                    &mut takeover_timers,
                    call_ref,
                    ctx.clock.now_ms(),
                    Some(skew_offset_ms),
                    ctx.config.keepalive_interval_sec * 1000,
                    None,
                );
                ctx.timers.restore(takeover_timers, call_ref.to_string()).await;
                let _ = ctx.txn.watch_self_release(call_ref).await;
            }
            Some(c)
        }
        // REBOOTED-PRIMARY on-demand reclaim (ADR-0014). An in-dialog request
        // (BYE / re-INVITE / UPDATE) can race the bulk `ReclaimAll` sweep on a
        // rebooted primary: the body sits fully reclaimable in `pri:{self}`
        // (the bootstrap imported it) but the serial sweep has not materialised
        // it yet — and the only other materialisation trigger was a backup's
        // reverse-flush `ReclaimCall` push, never an arriving request. Refusing
        // to look would 481 a healthy long-hold call whose state lives RIGHT
        // HERE. Materialise on demand, exactly as the reactive straggler path
        // does (timers restored, no smoothing — one call), under the per-call
        // guard the caller already holds; the bulk sweep's own
        // `materialize_if_absent` keeps the two passes idempotent. NOT a
        // takeover: this is our own call — no mark, no self-release watch. A
        // call never imported into `pri:{self}` (its only copy is the peer's
        // `bak:{self}`) still orphans — recovering THAT population needs an
        // on-demand pull from the peer (s11 CASE B, open).
        None => {
            let (call, skew_offset_ms) = ctx.state.peek_reclaimable(call_ref).await?;
            let mut timers = call.timers.clone();
            // Same restore-hygiene seam as the bulk/reactive reclaim paths:
            // re-anchor by the skew offset, drop the stale timeout, apply the
            // deep-past-due floor. No cohort (one call) → no smoothing.
            sanitize_restored_timers(
                &mut timers,
                call_ref,
                ctx.clock.now_ms(),
                Some(skew_offset_ms),
                ctx.config.keepalive_interval_sec * 1000,
                None,
            );
            if ctx.state.materialize_if_absent(call.clone()) {
                ctx.timers.restore(timers, call_ref.to_string()).await;
                ctx.metrics.bump_repl_reclaimed();
            }
            Some(call)
        }
    }
}

/// Run the synchronous rule chain for one in-dialog event, with the
/// MAX_MESSAGES_PER_CALL cap-defense wrapped around it.
///
/// EVERY in-dialog rule-chain event bumps the counter; if the bump crosses
/// `max_messages_per_call` and the handler did not itself terminate the call,
/// append a begin-termination so a runaway dialog (re-INVITE/OPTIONS storm,
/// glare loop, a peer that never stops) is torn down instead of processing
/// unbounded in-dialog events forever — each of which allocates a txn
/// (`set_txn`), a working `Call` clone, and a store body. Initial-INVITE and
/// the async limiter-refresh do NOT count. The bump rides the existing
/// per-event flush — `message_count` adds no extra replication traffic (it
/// mutates with the CSeq/state the event already changes). Order: bump +
/// capture `cap_exceeded` BEFORE the handler runs, terminate AFTER, so the
/// in-flight event (e.g. relaying this re-INVITE's response) is still serviced
/// before teardown.
fn rule_chain_turn(
    ctx: &Arc<RouterCtx>,
    mut call: Call,
    event: &CallEvent,
    res: &Resolution,
    call_ref: &str,
    now_ms: i64,
) -> HandlerResult {
    let bumped = call.message_count.unwrap_or(0) + 1;
    call.message_count = Some(bumped);
    let cap_exceeded = bumped > ctx.config.max_messages_per_call as i64
        && !matches!(
            call.state,
            CallModelState::Terminating | CallModelState::Terminated
        );
    let rule_ctx = RuleContext {
        call: RuleCall::new(&call),
        call_ref,
        event,
        source_leg_id: &res.source_leg_id,
        direction: res.direction,
        now_ms,
        config: &ctx.config,
    };
    let exec = ActionExecutor {
        config: &ctx.config,
        id_gen: &ctx.id_gen,
        now_ms,
    };
    let mut result = execute_rules(&ctx.rules, &call, &rule_ctx, &exec, &ctx.obligations);
    if cap_exceeded
        && !matches!(
            result.call.state,
            CallModelState::Terminating | CallModelState::Terminated
        )
    {
        // Tear the runaway call down through the standard executor so per-leg
        // BYE/CANCEL, dialog-tag ownership and the safety-timer contract apply
        // exactly as a rule-driven termination would. The RFC-3326 cause rides
        // the reason (must start with "SIP").
        let cap_ctx = RuleContext {
            call: RuleCall::new(&result.call),
            call_ref,
            event,
            source_leg_id: &res.source_leg_id,
            direction: res.direction,
            now_ms,
            config: &ctx.config,
        };
        // An UNANSWERED a-leg (still trying/early) has no final response yet:
        // `begin_termination` assumes the firing *rule* already replied (as
        // `setup-timeout` does via RespondToALeg) and so only settles the leg's
        // disposition. The cap fires from the router, not a rule, so nobody
        // replied — without this the caller's INVITE hangs until its own
        // Timer B and the limiter slot is held until the ~32 s
        // TerminatingTimeout. Send the 503 cap cause as the caller's final so
        // the INVITE resolves now and the call terminates (decrementing the
        // limiter) immediately. An answered a-leg (confirmed) takes the BYE
        // path inside begin_termination — no response then.
        let mut cap_actions = Vec::new();
        if matches!(result.call.a_leg.state, LegState::Trying | LegState::Early) {
            cap_actions.push(RuleAction::RespondToALeg {
                status: 503,
                reason: "Service Unavailable".into(),
                header_updates: vec![],
                contacts: vec![],
            });
        }
        cap_actions.push(RuleAction::BeginTermination {
            reason: Some("SIP;cause=503;text=\"message-cap-exceeded\"".into()),
        });
        let cap = exec.execute(&cap_actions, &result.call, &cap_ctx);
        result.call = cap.call;
        result.effects.critical.extend(cap.effects.critical);
        result.effects.outbound.extend(cap.effects.outbound);
        result.effects.soft.extend(cap.effects.soft);
        result.effects.buffered.extend(cap.effects.buffered);
        result.effects.fire_and_forget.extend(cap.effects.fire_and_forget);
        ctx.metrics.bump_message_cap_terminated();
    }
    result
}

/// Per-peer keepalive-timeout attribution (observability only;
/// `b2bua_peer_failures_total{...,kind="keepalive_timeout"}`). The genuine
/// no-200 keepalive timeout is the `KeepaliveTimeout` timer firing for a
/// specific leg L (`leg_id`): the `keepalive-timeout` rule tears L down (no BYE
/// to L — it is unresponsive) and BYEs the SURVIVING leg. So we MUST NOT
/// attribute to the outbound BYE's destination (that is the surviving, often
/// healthy, leg's hop and mis-classifies internal/external). Attribute to the
/// FAILED leg L's OWN egress-aware next hop — the exact hop the unanswered
/// OPTIONS went to. If L or its dialog can't be resolved we record nothing (no
/// fabricated address). Distinct from the reclaim-time stale drop
/// (`restore_hygiene`), which never reaches this event path.
fn record_keepalive_timeout_peer(ctx: &RouterCtx, event: &CallEvent, call: &Call) {
    if let CallEvent::Timer { timer_type: TimerType::KeepaliveTimeout, leg_id, .. } = event {
        if let Some((host, port)) = keepalive_timeout_peer(&ctx.config, call, leg_id.as_deref()) {
            use std::net::ToSocketAddrs;
            if let Ok(mut addrs) = (host.as_str(), port).to_socket_addrs() {
                if let Some(dest) = addrs.next() {
                    ctx.metrics.record_peer_failure(
                        &dest,
                        classify_b2bua_peer(&ctx.config, &dest),
                        crate::peer_failures::PeerFailureKind::KeepaliveTimeout,
                    );
                }
            }
        }
    }
}

/// Handle a `LimiterRefresh` timer: migrate every live hold to the current
/// window (an async `/v1/refresh` call), update the stored windows, and re-arm
/// the timer while the call is alive.
async fn handle_limiter_refresh(ctx: &Arc<RouterCtx>, mut call: Call, now_ms: i64) -> HandlerResult {
    let holds = crate::limiter::live_holds(&call);

    let mut fx = HandlerEffects::new();
    if holds.is_empty() {
        return HandlerResult { call, effects: fx };
    }

    // All holds migrate to the same current window; adopt it for every live
    // entry. On a backend failure `refresh` returns the holds unchanged, so the
    // windows simply stay put and we retry next cycle.
    let updated = ctx.limiter.refresh(&holds).await;
    if let Some(new_window) = updated.first().map(|h| h.window) {
        for e in call.limiter_entries.iter_mut() {
            if e.increment_succeeded != Some(false) {
                e.origin_window = new_window;
            }
        }
    }

    if call.state == CallModelState::Active {
        let entry = TimerEntry {
            id: format!("{:?}", TimerType::LimiterRefresh),
            timer_type: TimerType::LimiterRefresh,
            fire_at: now_ms + ctx.config.limiter_refresh_sec * 1000,
            leg_id: None,
        };
        call.timers =
            call::helpers::replace_timer_by_id(std::mem::take(&mut call.timers), entry.clone());
        fx.critical.push(CriticalStateEffect::ScheduleTimer(entry));
    }

    HandlerResult { call, effects: fx }
}

/// A request for a vanished call → 481 (ACK/responses are silently dropped).
async fn maybe_reject_orphan(ctx: &RouterCtx, event: &CallEvent) {
    if let CallEvent::Sip { message, src } = event {
        if let SipMessage::Request(req) = message.as_ref() {
            if req.method != "ACK" {
                let resp = generate_response(
                    req,
                    481,
                    "Call/Transaction Does Not Exist",
                    &GenerateResponseOpts::default(),
                );
                let _ = ctx.txn.send_response(resp, *src).await;
            }
        }
    }
}
