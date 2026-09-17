//! The per-call handler body: runs on the per-call FIFO with the state lock
//! held — reaper verdict gate, initial-INVITE admission, the in-dialog lookup
//! (resident, or materialised as a takeover / on-demand reclaim), the re-offer
//! of an unmatched datagram to the transactions the call seeded, the CANCEL
//! the layer matched nothing for, the rule chain, and the message-cap defense.

use std::net::SocketAddr;
use std::sync::Arc;

use call::helpers::cap_keepalive_fire_at;
use call::{Call, CallModelState, LegState, TerminationCause, TimerEntry, TimerType};
use sip_message::emergency::is_emergency_request;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::{Method, SipMessage};

use super::interpret::process_result;
use sip_txn::Reoffer;

use super::materialise::{materialise, reoffer_trigger, Materialised, Origin, Reason};
use super::peer_metrics::{classify_b2bua_peer, keepalive_timeout_peer};
use super::reclaim::discharge_as_own;
use super::release::{release_call, ReleaseKind};
use super::resolve::Resolution;
use super::responses::{build_481, build_store_fault_500};
use super::RouterCtx;
use crate::effects::{CriticalStateEffect, HandlerEffects, HandlerResult, QuietTurn};
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
            CallEvent::Sip { message, src, .. } => match message.as_ref() {
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
        let Some(call) = resident_or_materialised(ctx, &call_ref).await else {
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
        // A request in a dialog this call does not hold is refused before any
        // of the machinery below reads it (RFC 3261 §12.2.2).
        if let Some(refusal) = refuse_foreign_dialog(ctx, &call, &event, &res).await {
            record_refusal(ctx, call, &res.source_leg_id, &event, refusal.as_ref(), now_ms);
            return;
        }
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
        let own_tag = call::helpers::b2bua_tag(&call, &res.source_leg_id);
        if let Some(answer) = reject_stray_cancel(ctx, own_tag.as_deref(), &event).await {
            record_refusal(ctx, call, &res.source_leg_id, &event, Some(&answer), now_ms);
            return;
        }
        // The limiter-refresh timer is async (an HTTP call to migrate holds), so
        // it is handled outside the synchronous rule chain — like initial-INVITE.
        if matches!(&event, CallEvent::Timer { timer_type: TimerType::LimiterRefresh, .. }) {
            let before = call.clone();
            let res = handle_limiter_refresh(ctx, call, now_ms).await;
            crate::rules::invariants::enforce(
                &ctx.obligations,
                &before,
                crate::rules::invariants::finalize(res),
                now_ms,
                true,
            )
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
        let resp = crate::overload::build_reject_new_call_503(
            ctx.id_gen.new_tag(),
            req,
            decision.retry_after_sec,
        );
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

    let mut call = build_initial_call(req, src, &ctx.config, now_ms);
    if let Some(ring) = crate::message_ring::Ring::of(&ctx.config) {
        let a_leg = call.a_leg.leg_id.clone();
        call = ring.invite_received(call, &a_leg, req, now_ms);
    }
    ctx.state.create(call.clone());
    // RFC 3261 §8.1.1.3: a dialog-forming INVITE MUST carry a From tag. The
    // caller's From tag IS the a-leg dialog's remote tag, so admitting a
    // tag-less INVITE would seed an un-probeable a-leg dialog (its in-dialog
    // keepalive OPTIONS could never be built — see `send_request_to_leg`),
    // producing the "OPTIONS to called, not calling" asymmetry that also
    // round-trips through HA hydration. Reject malformed at ingest instead.
    // Created-then-rejected mirrors the decision-reject path so the Terminated
    // invariant reaps the call + propagates the delete.
    let mut handled = if call.a_leg.from_tag.is_empty() {
        let a_invite = crate::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite);
        crate::initial_invite::reject_call(
            call.clone(),
            &a_invite,
            400,
            Some("Bad Request - missing From tag".into()),
            None,
            &[],
            &ctx.id_gen,
            now_ms,
            TerminationCause::Admission,
        )
    } else {
        // `req.image()` is the datagram this INVITE arrived as — the only place
        // it exists (the call carries a header snapshot, not bytes), and what a
        // trace activation backfills its `sip.in` from.
        handle_initial_invite(
            call.clone(),
            ctx.decision.as_ref(),
            ctx.limiter.as_ref(),
            &ctx.config,
            &ctx.id_gen,
            &ctx.wire_faults,
            &ctx.services,
            req.image(),
            &ctx.clock,
            now_ms,
        )
        .await
    };
    // ── Decision-application drop guard (069) ───────────────────────────────
    // The caller CANCELed while this turn was queued or parked on its decision
    // round trip: the txn layer already finalized the a-leg INVITE (200 + 487 —
    // the transaction's ONE final, RFC 3261 §17.2.1) and the `Cancelled` event
    // is queued right behind this turn. Whatever this turn resolved — route,
    // reject (decision-authored OR the malformed-INVITE 400 above), redirect,
    // relay, error — is moot: drop the result whole. No b-leg launch, no
    // authored final; the queued `handle-cancel` turn owns the termination (its
    // Cancel CDR is what keeps the ADR-0022 X2 synthesis silent). The mark
    // write (ingress run loop) and this read are not ordered by the per-call
    // lock, so a marking that lands after this check reverts to the pre-drop
    // path for that scheduler sliver — the b-leg is launched then CANCELed
    // (§9.1-held until a provisional), indistinguishable from the unavoidable
    // wire race.
    if ctx.state.is_setup_cancelled(call_ref) {
        ctx.metrics.bump_decision_dropped_cancelled();
        tracing::debug!(
            %call_ref,
            "decision result dropped: caller CANCELed during the decision round trip"
        );
        handled = dropped_on_setup_cancel(&call, handled);
    }
    Turn::Result(crate::rules::invariants::enforce(
        &ctx.obligations,
        &call,
        crate::rules::invariants::finalize(handled),
        now_ms,
        true,
    ))
}

/// The 069 drop result: the pre-handler call — no b-leg, no wire effects —
/// carrying ONLY what the discarded turn already committed *outside* the call.
/// The limiter holds `apply_route` INCRemented ride over so the queued
/// termination's standard obligation discharge still DECRs each one
/// (the INCR↔DECR pairing survives the drop), and the trace stamp rides over
/// so a sampled call's later turns (the CANCEL, the 487, the termination that
/// closes its registry entry) still emit into its trace.
fn dropped_on_setup_cancel(pre: &Call, handled: HandlerResult) -> HandlerResult {
    let mut call = pre.clone();
    call.limiter_entries = handled.call.limiter_entries;
    call.trace_id = handled.call.trace_id;
    call.root_span_id = handled.call.root_span_id;
    call.sampled = handled.call.sampled;
    HandlerResult { call, effects: HandlerEffects::new() }
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
    if let CallEvent::Sip { message, src, .. } = event {
        if let SipMessage::Request(req) = message.as_ref() {
            if ctx.store_faults.check(StoreFaultPoint::LiveInDialog).is_err() {
                if req.method() != "ACK" {
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
            // we could not fetch, and held to the same ledger ceiling. Runtime
            // driver only: the serialized `call.timers` intent stays untouched
            // (the call is untouched), which is safe — a later HA restore
            // sanitizes past-due entries.
            let interval_ms = ctx.config.keepalive_interval_sec * 1000;
            let entry = TimerEntry {
                id: TimerType::Keepalive.timer_id(None),
                timer_type: TimerType::Keepalive,
                fire_at: cap_keepalive_fire_at(now_ms + interval_ms, now_ms, interval_ms),
                leg_id: None,
            };
            ctx.timers.schedule(entry, call_ref.to_string()).await;
            return true;
        }
    }
    false
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
        Materialised::Served(call) | Materialised::Resident(call) => Some(call),
        Materialised::Refused(_) => None,
    }
}

/// RFC 3261 §12.2.2 for a mid-dialog request on a live call: a To-tag naming
/// no dialog this stack holds on the leg it arrived on draws 481 and touches
/// nothing. An ACK draws no response at all (§17.1.1.3) and is dropped, so the
/// §13.3.1.4 ladder keeps repeating the 2xx it failed to acknowledge. A CANCEL
/// is matched by transaction (§9.1) and never read here. `Some` when refused,
/// holding the 481 sent, if any.
async fn refuse_foreign_dialog(
    ctx: &RouterCtx,
    call: &Call,
    event: &CallEvent,
    res: &Resolution,
) -> Option<Option<sip_message::SipResponse>> {
    let CallEvent::Sip { message, src, .. } = event else { return None };
    let SipMessage::Request(req) = message.as_ref() else { return None };
    if req.method() == Method::Cancel {
        return None;
    }
    let tag = req.to().tag()?;
    if call::helpers::holds_local_tag(call, &res.source_leg_id, tag) != Some(false) {
        return None;
    }
    if req.method() == Method::Ack {
        return Some(None);
    }
    let refusal = build_481(req, None);
    let _ = ctx.txn.send_response(refusal.clone(), *src).await;
    Some(Some(refusal))
}

/// RFC 3261 §9.2 for a CANCEL that matched no INVITE transaction in the layer
/// and none a call here could rebuild: 481 under `to_tag`, the tag the call's
/// final to the INVITE carried where a call resolves, and no effect on any
/// call. The 481 sent when `event` was such a CANCEL.
pub(super) async fn reject_stray_cancel(
    ctx: &RouterCtx,
    to_tag: Option<&str>,
    event: &CallEvent,
) -> Option<sip_message::SipResponse> {
    let CallEvent::Sip { message, src, .. } = event else { return None };
    let SipMessage::Request(req) = message.as_ref() else { return None };
    if req.method() != Method::Cancel {
        return None;
    }
    let refusal = build_481(req, to_tag);
    let _ = ctx.txn.send_response(refusal.clone(), *src).await;
    Some(refusal)
}

/// A request refused on a live call's behalf is still a message of the
/// call's: the ring records it and the answer, and the record lands.
fn record_refusal(
    ctx: &RouterCtx,
    call: Call,
    leg_id: &str,
    event: &CallEvent,
    answer: Option<&sip_message::SipResponse>,
    now_ms: i64,
) {
    let Some(ring) = crate::message_ring::Ring::of(&ctx.config) else { return };
    let CallEvent::Sip { message, .. } = event else { return };
    let SipMessage::Request(req) = message.as_ref() else { return };
    ctx.state.update(ring.refused(call, leg_id, req, answer, now_ms));
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
fn rule_chain_turn(
    ctx: &Arc<RouterCtx>,
    mut call: Call,
    event: &CallEvent,
    res: &Resolution,
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
    let bumped = match event {
        CallEvent::Timer { timer_type: TimerType::Keepalive, .. } => 0,
        CallEvent::Timer { timer_type: TimerType::Rung { .. }, .. } => {
            call.message_count.unwrap_or(0)
        }
        _ => call.message_count.unwrap_or(0) + 1,
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
    if let CallEvent::Timer { timer_type: TimerType::Rung { obligation }, .. } = event {
        let before = call.clone();
        exec.repeat(&mut call, &mut ladder_fx, obligation);
        ladder_fx.quiet = Some(QuietTurn::OwnRung);
        let repeated = HandlerResult { call, effects: ladder_fx };
        return crate::rules::invariants::enforce(
            &ctx.obligations,
            &before,
            crate::rules::invariants::finalize(repeated),
            now_ms,
            true,
        );
    }
    let discharged = exec.discharge(&mut call, &mut ladder_fx, event, &res.source_leg_id);
    // A non-2xx INVITE final ends the transaction it answers (RFC 3261
    // §17.1.1.3, hop-ACKed below the TU): the record stops naming that round
    // as in flight before the rules read it.
    if let CallEvent::Sip { message, .. } = event {
        if let SipMessage::Response(resp) = message.as_ref() {
            if resp.status() >= 300 && resp.cseq().method() == Method::Invite {
                if let Some(branch) = resp.top_via().branch() {
                    call = call::helpers::close_rejected_invite_round(
                        call,
                        &res.source_leg_id,
                        branch,
                    );
                }
            }
        }
    }
    // The transaction layer answered an out-of-dialog CANCEL itself — 200 to
    // the CANCEL, 487 to the initial INVITE (RFC 3261 §9.2): the record
    // carries that transaction's one final before the rules read it.
    if let CallEvent::Cancelled { in_dialog: false, .. } = event {
        call = call::helpers::record_invite_final(call, &res.source_leg_id, 487);
    }
    if let CallEvent::Timer { timer_type: TimerType::RepeatGiveUp { obligation }, .. } = event {
        ctx.metrics.record_repeat_give_up(obligation.kind());
        exec.give_up(&mut call, &mut ladder_fx, obligation);
    }
    // The record names the message before the rules read it, so what the
    // turn sends follows what it received.
    if let Some(ring) = crate::message_ring::Ring::of(&ctx.config) {
        call = ring.received(call, &res.source_leg_id, event, discharged.as_ref(), now_ms);
    }
    // A decision folding back is marked before the rules apply it, whichever
    // rule claims the fold.
    call = crate::decision_log::fold_decided(call, event, now_ms);
    let rule_ctx = RuleContext {
        call: RuleCall::new(&call),
        call_ref,
        event,
        source_leg_id: &res.source_leg_id,
        direction: res.direction,
        now_ms,
        config: &ctx.config,
        discharged: discharged.as_ref(),
    };
    let mut result = execute_rules(&ctx.rules, &call, &rule_ctx, &exec, &ctx.obligations);
    // A re-ACK leaves the body as the rules read it; a rule that also wrote
    // into it (a CDR event, a disposition) made the turn a write.
    if result.effects.quiet == Some(QuietTurn::ReAck) && result.call != call {
        result.effects.quiet = None;
    }
    // The ladder's own effects (its cancels) precede the rule's.
    ladder_fx.extend(std::mem::take(&mut result.effects));
    result.effects = ladder_fx;
    if let CallEvent::Timer { timer_type: TimerType::RepeatGiveUp { obligation }, .. } = event {
        let before = result.call.clone();
        let settled = exec.settle_give_up(result, obligation, &rule_ctx);
        result = crate::rules::invariants::enforce(
            &ctx.obligations,
            &before,
            crate::rules::invariants::finalize(settled),
            now_ms,
            true,
        );
    }
    if cap_exceeded
        && !matches!(result.call.state, CallModelState::Terminating | CallModelState::Terminated)
    {
        // Tear the runaway call down through the standard executor so per-leg
        // BYE/CANCEL, dialog-tag ownership and the safety-timer contract apply
        // exactly as a rule-driven termination would, and close the turn with
        // the terminal invariants as a rule-driven termination does: a call
        // whose legs are all resolved here is `Terminated` — CDR written,
        // limiter released, removed — in this turn. The RFC-3326 cause rides
        // the reason (must start with "SIP").
        let cap_ctx = RuleContext {
            call: RuleCall::new(&result.call),
            call_ref,
            event,
            source_leg_id: &res.source_leg_id,
            direction: res.direction,
            now_ms,
            config: &ctx.config,
            discharged: discharged.as_ref(),
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
            cause: TerminationCause::MessageCap,
            by_leg: None,
        });
        let before = result.call.clone();
        let cap = exec.execute(&cap_actions, &result.call, &cap_ctx);
        result.call = cap.call;
        result.effects.critical.extend(cap.effects.critical);
        result.effects.outbound.extend(cap.effects.outbound);
        result.effects.soft.extend(cap.effects.soft);
        result.effects.buffered.extend(cap.effects.buffered);
        result.effects.fire_and_forget.extend(cap.effects.fire_and_forget);
        ctx.metrics.bump_message_cap_terminated();
        result = crate::rules::invariants::enforce(
            &ctx.obligations,
            &before,
            crate::rules::invariants::finalize(result),
            now_ms,
            true,
        );
    }
    result
}

/// Per-peer keepalive-timeout attribution (observability only;
/// `b2bua_peer_failures_total{...,kind="keepalive_timeout"}`). The genuine
/// no-200 keepalive timeout is the `KeepaliveTimeout` timer firing for a
/// specific leg L (`leg_id`): the `keepalive-timeout` rule ends the call and
/// BYEs BOTH legs. So we MUST NOT attribute to an outbound BYE's destination
/// (the first is the a-leg's hop, often the healthy one, and mis-classifies
/// internal/external). Attribute to the FAILED leg L's OWN egress-aware next
/// hop — the exact hop the unanswered OPTIONS went to. If L or its dialog can't be resolved we record nothing (no
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
                    // Aggregated per hop: a dead peer is one episode, however
                    // many calls it takes with it (ADR-0026).
                    ctx.keepalive_waves.record(&dest.to_string(), "timeouts", 1);
                }
            }
        }
    }
}

/// Handle a `LimiterRefresh` timer: migrate every live hold to the current
/// window (an async `/v1/refresh` call), update the stored windows, and re-arm
/// the timer while the call is alive.
async fn handle_limiter_refresh(
    ctx: &Arc<RouterCtx>,
    mut call: Call,
    now_ms: i64,
) -> HandlerResult {
    let holds = crate::limiter::live_holds(&call);

    let mut fx = HandlerEffects::new();
    if holds.is_empty() {
        return HandlerResult { call, effects: fx };
    }

    // All holds migrate to the same current window; adopt it for every live
    // entry. On a backend failure `refresh` returns the holds unchanged, so the
    // windows simply stay put and we retry next cycle.
    let updated = ctx.limiter.refresh(&holds).await;
    if crate::trace::sampled(&call) {
        crate::trace::emit::limiter(
            &call,
            now_ms,
            "refresh",
            &format!("{} hold(s) -> window {:?}", holds.len(), updated.first().map(|h| h.window)),
        );
    }
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
    if let CallEvent::Sip { message, src, .. } = event {
        if let SipMessage::Request(req) = message.as_ref() {
            if req.method() != "ACK" {
                let _ = ctx.txn.send_response(build_481(req, None), *src).await;
            }
        }
    }
}
