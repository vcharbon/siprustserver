//! `SipRouter` — consumes the transaction-layer event stream + the timer fire
//! channel, resolves each event's `callRef` (synchronously), dispatches the
//! handler body to the per-call FIFO, and interprets the typed effects in the
//! fixed order (persist → critical → outbound → soft → buffered). Port of the
//! load-bearing half of `SipRouter.ts` (`routeKey` + `withCall` + `processResult`).

use std::net::SocketAddr;
use std::sync::Arc;

use call::{Call, CallModelState, CdrEvent, CdrEventType, Direction, TimerEntry, TimerType};
use repl_net::frame::Watermark;
use sip_clock::Clock;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::message_helpers::parse_uri_params;
use sip_message::{serialize, SipMessage};
use sip_txn::{IdGen, TransactionLayer};
use tokio::sync::mpsc;

use crate::cdr::CdrWriter;
use crate::config::B2buaConfig;
use crate::decision::{CallDecisionEngine, CallReferResponse};
use crate::dispatch::PerCallDispatcher;
use crate::effects::{
    BufferedObservabilityEffect, CriticalStateEffect, FireAndForgetEffect, HandlerEffects,
    HandlerResult, OutboundBody, OutboundTxnMode, SoftBoundedEffect,
};
use crate::event::CallEvent;
use crate::initial_invite::{build_initial_call, handle_initial_invite};
use crate::limiter::CallLimiter;
use crate::metrics::B2buaMetrics;
use crate::repl::{Readiness, ReadinessState};
use crate::rules::{execute_rules, ActionExecutor, RuleContext, RuleDefinition};
use crate::store::CallState;
use crate::timers::TimerService;

/// Everything a handler body + the interpreter need. Shared via `Arc`.
pub struct RouterCtx {
    pub config: B2buaConfig,
    pub state: CallState,
    pub txn: TransactionLayer,
    pub timers: TimerService,
    pub dispatcher: PerCallDispatcher,
    pub decision: Arc<dyn CallDecisionEngine>,
    pub limiter: Arc<dyn CallLimiter>,
    pub cdr: Arc<dyn CdrWriter>,
    pub id_gen: Arc<IdGen>,
    pub clock: Clock,
    pub rules: Arc<Vec<RuleDefinition>>,
    pub metrics: B2buaMetrics,
    /// Self-reported readiness driving the OPTIONS health responder (S7). The
    /// default/legacy path uses [`Readiness::always_ready`] → always 200.
    pub readiness: Readiness,
    /// Re-entrant event sink: fire-and-forget work (the async `/call/refer`
    /// round-trip) folds its result back into the router by sending a
    /// `CallEvent::InternalEvent` here, which `run` consumes via `on_event` —
    /// keeping re-entry single-threaded and out of a non-`Send` async cycle.
    pub reentry_tx: mpsc::UnboundedSender<CallEvent>,
}

/// Replication-driven commands the puller/supervisor inject into the router loop
/// (ADR-0011 X11 fail-back). Routed through the same single-threaded `run` loop
/// as SIP events so reclaim/handback never races the per-call handlers.
#[derive(Debug, Clone)]
pub enum ReplCommand {
    /// **Go-active bulk reclaim** — materialise every `pri:{self}` call into the
    /// live map + re-arm timers (a rebooted primary re-*serving* its partition,
    /// not just re-storing it). Fired once the supervisor reports bootstrap-complete.
    ReclaimAll,
    /// **Reactive reclaim** of one call a backup just reverse-flushed to us — the
    /// flip-race straggler an acting-backup took over *after* the bulk sweep.
    ReclaimCall(String),
    /// **Handback** — a reclaiming primary told us (its backup) to deactivate
    /// every takeover copy for `primary` whose reverse-flush position it has
    /// pulled past (`<= as_of`, in our changelog domain). See [`ReplServer`].
    Deactivate { primary: String, as_of: Watermark },
    /// **Eager takeover** — the supervisor observed `primary` leave membership
    /// (`MemberDelta::Removed`). Materialise its entire `bak:{primary}` partition
    /// into the live map NOW (don't wait for an inbound in-dialog request), so a
    /// quiescent failed-over dialog keeps its keepalive running on the survivor.
    /// See [`take_over_peer`].
    TakeOverPeer { primary: String },
}

/// How an event resolves to a call + the leg it arrived on.
struct Resolution {
    call_ref: Option<String>,
    source_leg_id: String,
    direction: Direction,
    initial_invite: bool,
}

/// Run the router loop over the txn-event + timer-fire channels until both close.
pub async fn run(
    ctx: Arc<RouterCtx>,
    mut txn_rx: mpsc::Receiver<sip_txn::TransactionEvent>,
    mut timer_rx: mpsc::UnboundedReceiver<CallEvent>,
    mut reentry_rx: mpsc::UnboundedReceiver<CallEvent>,
    mut repl_rx: mpsc::UnboundedReceiver<ReplCommand>,
) {
    loop {
        tokio::select! {
            ev = txn_rx.recv() => match ev {
                Some(ev) => on_event(&ctx, CallEvent::from_txn(ev)).await,
                None => break,
            },
            ev = timer_rx.recv() => {
                if let Some(ev) = ev {
                    on_event(&ctx, ev).await;
                }
            },
            ev = reentry_rx.recv() => {
                if let Some(ev) = ev {
                    on_event(&ctx, ev).await;
                }
            },
            cmd = repl_rx.recv() => {
                if let Some(cmd) = cmd {
                    on_repl_command(&ctx, cmd).await;
                }
            },
        }
    }
}

/// Interpret a replication-driven [`ReplCommand`] (ADR-0011 X11 fail-back).
async fn on_repl_command(ctx: &Arc<RouterCtx>, cmd: ReplCommand) {
    match cmd {
        ReplCommand::ReclaimAll => {
            for call in ctx.state.reclaim_scan().await {
                reclaim_into_live(ctx, call).await;
            }
        }
        ReplCommand::ReclaimCall(call_ref) => {
            if let Some(call) = ctx.state.peek_reclaimable(&call_ref).await {
                reclaim_into_live(ctx, call).await;
            }
        }
        ReplCommand::Deactivate { primary, as_of } => {
            deactivate_takeovers(ctx, &primary, as_of).await;
        }
        ReplCommand::TakeOverPeer { primary } => {
            take_over_peer(ctx, &primary).await;
        }
    }
}

/// **Eager death-triggered takeover** (ADR-0011 X11): materialise every call in
/// the dead `primary`'s `bak:{primary}` partition into the live map and re-arm its
/// timers — the survivor's response to a `MemberDelta::Removed` for `primary`.
///
/// This closes the **quiescent long-hold** loss: a call parked in the B2BUA
/// OPTIONS-keepalive loop sends nothing, so the reactive
/// [`hydrate_from_replica`](crate::store::CallState::hydrate_from_replica) — which
/// only fires on an inbound in-dialog request — never triggers. Its keepalive
/// timer dies with the primary and is re-armed on no live node, so ~one interval
/// later the UAC's mandatory keepalive OPTIONS never arrives and the dialog is
/// torn down as failed. Taking the partition over eagerly on the death signal
/// keeps the survivor probing it.
///
/// Per call (each under the per-call lock, like [`reclaim_into_live`]):
/// 1. `mark_takeover` — record it as an acting-backup copy so the X11
///    `Deactivate` handback can later resolve double-ownership.
/// 2. `materialize_if_absent` — insert into the live map iff not already resident
///    (idempotent: a call the reactive path already hydrated is left untouched).
/// 3. **Restore the call's ORIGINAL timer deadlines** (`call.timers`, which IS
///    replicated). Unlike [`reclaim_into_live`] — which *defers* the keepalive a
///    fresh interval so a rebooting primary doesn't double-probe during the
///    handback window — the survivor is the call's SOLE keeper here and MUST probe
///    on the original schedule: a past-due keepalive fires immediately (then
///    re-arms on the next interval), and if the primary never reboots the call
///    still stays alive. Deferring would re-open the very gap this fixes (and lose
///    the call outright on permanent node loss). The rebooting primary's reclaim
///    keeps deferring, so during the brief double-ownership window only the
///    survivor probes — no CSeq collision.
/// 4. `flush` — a **reverse** flush (the call_ref names the dead primary, so
///    `store_target` resolves the acting-backup Reverse plan) that bumps this
///    node's changelog position for `primary`. Without it `position_of` stays
///    `None` and the X11 handback can never fire (`deactivate_targets` skips it)
///    → permanent double-serve. A quiescent call mutates nothing, so this explicit
///    flush is the ONLY thing that establishes the handback `as_of`.
async fn take_over_peer(ctx: &Arc<RouterCtx>, primary: &str) {
    for call in ctx.state.reclaim_backup_scan(primary).await {
        let call_ref = call.call_ref.clone();
        let _guard = ctx.state.lock(&call_ref).await;
        let timers = call.timers.clone();
        // `materialize_if_absent` is the gate: a call the reactive path already
        // hydrated is left untouched (it is already marked + flushed + timed).
        if ctx.state.materialize_if_absent(call.clone()) {
            ctx.state.mark_takeover(&call_ref);
            ctx.timers.restore(timers, call_ref.clone()).await;
            // Reverse-flush so the rebooting primary can later hand this copy back.
            ctx.state.flush(&call);
            ctx.metrics.bump_repl_eager_takeover();
        }
    }
}

/// Materialise one reclaimed call into the live map + re-arm its timers — the
/// active reclaim that makes a rebooted primary re-*serve* (ADR-0011 X11). The
/// **keepalive** timer is re-armed a *fresh* interval out (not its stale absolute
/// deadline) so the primary defers its first in-dialog OPTIONS until well past
/// the `Deactivate` burst — the backup hands its copy back before either node
/// probes the leg, closing the double-OPTIONS/CSeq-collision window. The
/// `GlobalDuration` cap and every other timer keep their absolute deadlines.
async fn reclaim_into_live(ctx: &Arc<RouterCtx>, mut call: Call) {
    let call_ref = call.call_ref.clone();
    // Hold the per-call state lock across materialise + timer re-arm, exactly as
    // `process` and `deactivate_takeovers` do, so a concurrent dispatcher handler
    // for this call_ref cannot interleave and double-arm (or clobber the deferred
    // keepalive below).
    let _guard = ctx.state.lock(&call_ref).await;
    let now_ms = ctx.clock.now_ms();
    let interval_ms = ctx.config.keepalive_interval_sec * 1000;
    // Re-arm the keepalive a *fresh* interval out (not its stale absolute
    // deadline) so the primary defers its first in-dialog OPTIONS past the
    // `Deactivate` burst — and write the new deadline back into the call we
    // store, so a flush before that first keepalive cannot re-replicate a
    // past-due deadline (which a later hydrate would fire immediately). Every
    // other timer keeps its absolute deadline.
    for t in call.timers.iter_mut() {
        if matches!(t.timer_type, TimerType::Keepalive) {
            t.fire_at = now_ms + interval_ms;
        }
    }
    let timers = call.timers.clone();
    if ctx.state.materialize_if_absent(call) {
        ctx.timers.restore(timers, call_ref).await;
        ctx.metrics.bump_repl_reclaimed();
    }
}

/// Hand back every takeover copy for `primary` whose reverse-flush position the
/// primary has pulled past (`<= as_of`) (ADR-0011 X11 `Deactivate`) — i.e. the
/// primary provably holds and serves it now. Each is a **local-only** teardown —
/// drop the live copy + cancel its timers/txns/dispatch — with a `ghost-backup`
/// CDR end-event (a deactivation, *not* a real hangup) and the handback counter;
/// it propagates **no** delete (the call lives on at `primary`, which keeps
/// forward-refreshing this node's backup `Element`).
async fn deactivate_takeovers(ctx: &Arc<RouterCtx>, primary: &str, as_of: Watermark) {
    for call_ref in ctx.state.deactivate_targets(primary, as_of) {
        let _guard = ctx.state.lock(&call_ref).await;
        let now_ms = ctx.clock.now_ms();
        // Snapshot under the lock *before* tearing down (drop_local removes it),
        // but only emit the ghost-backup CDR + bump the handback counter if a
        // live copy was actually dropped — so the CDR and the metric stay a
        // matched pair and we never CDR a deactivation that did not happen.
        let snapshot = ctx.state.peek(&call_ref);
        if ctx.state.drop_local(&call_ref) {
            if let Some(mut call) = snapshot {
                call.cdr_events.push(CdrEvent {
                    event_type: CdrEventType::Bye,
                    timestamp: now_ms,
                    leg_id: call.a_leg.leg_id.clone(),
                    status_code: None,
                    reason: Some("ghost-backup".into()),
                });
                ctx.cdr.write(&call, now_ms).await;
            }
            ctx.timers.cancel_all(call_ref.clone()).await;
            ctx.txn.cancel_txns_for_call(&call_ref).await;
            ctx.dispatcher.enqueue_poison(&call_ref);
            ctx.metrics.bump_repl_handback();
        }
    }
}

async fn on_event(ctx: &Arc<RouterCtx>, event: CallEvent) {
    // Per-method / per-(method,code) data-path counters. Every inbound SIP
    // message lands here once, so this is the single chokepoint to meter them.
    if let CallEvent::Sip { message, .. } = &event {
        match message.as_ref() {
            SipMessage::Request(req) => ctx.metrics.record_request(&req.method),
            SipMessage::Response(resp) => ctx.metrics.record_response(&resp.cseq.method, resp.status),
        }
    }

    // Out-of-dialog OPTIONS keepalive: self-report readiness (S7, ADR-0011 X6).
    // The front proxy probe keys on the status + Reason header text
    // (`sip-proxy::health::probe::classify_503`).
    if let CallEvent::Sip { message, src } = &event {
        if let SipMessage::Request(req) = message.as_ref() {
            if req.method.eq_ignore_ascii_case("OPTIONS") && req.to.tag.is_none() {
                let resp = build_options_health_response(&ctx.readiness, &ctx.id_gen, req);
                ctx.txn.send_response(resp, *src).await;
                return;
            }
        }
    }

    let mut res = resolve(ctx, &event);
    if res.call_ref.is_none() {
        // Acting-backup takeover fallback (production resolution path). A real
        // UAC routes in-dialog requests through the proxy (stickiness cookie in
        // the Route header, no `callref` request-URI param), so a pure backup —
        // which never primary-served the call — can key the dialog neither from
        // the message nor from its empty in-memory `sip_index`. Recover the
        // callRef from the replica store's SIP index (the puller imported it)
        // before declaring the event unroutable. Without this the failed-over
        // BYE is silently dropped and the dialog never terminates on the backup.
        res.call_ref = replica_takeover_call_ref(ctx, &event).await;
    }
    let call_ref = match res.call_ref.clone() {
        Some(r) => r,
        None => {
            ctx.metrics.bump_unroutable_dropped();
            return;
        }
    };

    let ctx2 = ctx.clone();
    ctx.dispatcher.dispatch(
        &call_ref,
        Box::pin(async move {
            process(&ctx2, event, res).await;
        }),
    );
}

/// Build the self-reported readiness reply to an out-of-dialog OPTIONS
/// keepalive (S7). Every reply mints a local To-tag: RFC 3261 §8.2.6.2 requires
/// a To-tag on any response > 100 to an out-of-dialog request (the 2xx path
/// always did; the 503 path needs it too, and `hydrate_response` rejects a
/// tagless response otherwise). The status + `Reason` header text is the
/// contract `sip-proxy::health::probe::classify_503` keys on:
///   - `Ready`    → `200 OK`.
///   - `NotReady` → `503` + `Reason: SIP;cause=503;text="not-ready"`.
///   - `Draining` → `503` + `Reason: SIP;cause=503;text="draining"` +
///     `Retry-After: 0`.
pub(crate) fn build_options_health_response(
    readiness: &Readiness,
    id_gen: &IdGen,
    req: &sip_message::SipRequest,
) -> sip_message::SipResponse {
    use sip_message::types::SipHeader;

    let hdr = |name: &str, value: &str| SipHeader {
        name: name.to_string(),
        value: value.to_string(),
    };

    let (status, reason, extra_headers): (u16, &str, Vec<SipHeader>) = match readiness.state() {
        ReadinessState::Ready => (200, "OK", Vec::new()),
        ReadinessState::NotReady => (
            503,
            "Service Unavailable",
            vec![hdr("Reason", "SIP;cause=503;text=\"not-ready\"")],
        ),
        ReadinessState::Draining => (
            503,
            "Service Unavailable",
            vec![
                hdr("Reason", "SIP;cause=503;text=\"draining\""),
                hdr("Retry-After", "0"),
            ],
        ),
    };

    generate_response(
        req,
        status,
        reason,
        &GenerateResponseOpts {
            to_tag: Some(id_gen.new_tag()),
            extra_headers,
            ..Default::default()
        },
    )
}

/// Resolve the `callRef` + source leg for an event (synchronous, no blocking).
fn resolve(ctx: &RouterCtx, event: &CallEvent) -> Resolution {
    match event {
        CallEvent::Sip { message, .. } => match message.as_ref() {
            SipMessage::Request(req) => {
                if req.method.eq_ignore_ascii_case("INVITE") && req.to.tag.is_none() {
                    let call_ref = call::derive_call_ref(
                        &ctx.config.self_ordinal,
                        &req.call_id,
                        req.from.tag.as_deref().unwrap_or(""),
                    );
                    return Resolution {
                        call_ref: Some(call_ref),
                        source_leg_id: "a".into(),
                        direction: Direction::FromA,
                        initial_invite: true,
                    };
                }
                // In-dialog request: read our cr/lg from the Request-URI params.
                // NB `parse_uri_params` lower-cases param NAMES (URI params are
                // case-insensitive per RFC 3261 §19.1.1), so the stamped `callRef`
                // is keyed as `callref`. The primary path masks a mismatch via the
                // in-memory `sip_index` fallback below; the acting-backup takeover
                // path has no such index, so the param IS the only key — read it by
                // its normalised (lower-case) name.
                let params = parse_uri_params(&req.uri);
                let leg = params
                    .get("leg")
                    .map(|v| crate::stack_identity::decode_param(v))
                    .unwrap_or_else(|| "a".into());
                let call_ref = params
                    .get("callref")
                    .map(|v| crate::stack_identity::decode_param(v))
                    .or_else(|| {
                        ctx.state.resolve_from_sip_key_sync(
                            &req.call_id,
                            req.from.tag.as_deref().unwrap_or(""),
                        )
                    });
                Resolution {
                    direction: leg_direction(ctx, call_ref.as_deref(), &leg),
                    call_ref,
                    source_leg_id: leg,
                    initial_invite: false,
                }
            }
            SipMessage::Response(resp) => {
                // Response: read our cr/lg from the top Via we stamped.
                let (cr, lg) = via_cr_lg(resp.headers.first().map(|h| h.value.as_str()))
                    .or_else(|| {
                        resp.headers
                            .iter()
                            .find(|h| h.name.eq_ignore_ascii_case("via"))
                            .and_then(|h| via_cr_lg(Some(&h.value)))
                    })
                    .unwrap_or((None, "a".into()));
                let call_ref = cr.or_else(|| {
                    ctx.state.resolve_from_sip_key_sync(&resp.call_id, resp.to.tag.as_deref().unwrap_or(""))
                });
                Resolution {
                    direction: leg_direction(ctx, call_ref.as_deref(), &lg),
                    call_ref,
                    source_leg_id: lg,
                    initial_invite: false,
                }
            }
        },
        CallEvent::Cancelled { call_id, from_tag } => {
            let call_ref = ctx.state.resolve_from_sip_key_sync(call_id, from_tag);
            Resolution {
                call_ref,
                source_leg_id: "a".into(),
                direction: Direction::FromA,
                initial_invite: false,
            }
        }
        CallEvent::Timeout { call_ref, leg_id, .. } => {
            let leg = leg_id.clone().unwrap_or_else(|| "a".into());
            Resolution {
                direction: leg_direction(ctx, call_ref.as_deref(), &leg),
                call_ref: call_ref.clone(),
                source_leg_id: leg,
                initial_invite: false,
            }
        }
        CallEvent::Timer { call_ref, leg_id, .. } => {
            let leg = leg_id.clone().unwrap_or_else(|| "a".into());
            Resolution {
                direction: leg_direction(ctx, Some(call_ref), &leg),
                call_ref: Some(call_ref.clone()),
                source_leg_id: leg,
                initial_invite: false,
            }
        }
        CallEvent::InternalEvent { call_ref, .. } => Resolution {
            call_ref: Some(call_ref.clone()),
            source_leg_id: "a".into(),
            direction: Direction::FromA,
            initial_invite: false,
        },
    }
}

/// Recover the takeover `callRef` for an in-dialog SIP request from the replica
/// store's SIP index (the acting-backup production path). Only in-dialog requests
/// (those carrying a To-tag) are candidates; an initial request, a response, or a
/// non-SIP event is never a dialog takeover. `None` when not applicable or no
/// replica matches — the caller then treats the event as unroutable.
async fn replica_takeover_call_ref(ctx: &RouterCtx, event: &CallEvent) -> Option<String> {
    let CallEvent::Sip { message, .. } = event else { return None };
    let SipMessage::Request(req) = message.as_ref() else { return None };
    if req.to.tag.is_none() {
        return None; // initial request — a brand-new dialog, not a takeover
    }
    ctx.state
        .resolve_from_replica_index(&req.call_id, req.from.tag.as_deref().unwrap_or(""))
        .await
}

fn leg_direction(_ctx: &RouterCtx, _call_ref: Option<&str>, leg: &str) -> Direction {
    if leg == "a" {
        Direction::FromA
    } else {
        Direction::FromB
    }
}

/// The per-call handler body: check the call out, run the handler, interpret.
async fn process(ctx: &Arc<RouterCtx>, event: CallEvent, res: Resolution) {
    let call_ref = res.call_ref.clone().expect("dispatched events carry a callRef");
    let _guard = ctx.state.lock(&call_ref).await;
    let now_ms = ctx.clock.now_ms();

    let result = if res.initial_invite {
        if ctx.state.peek(&call_ref).is_some() {
            return; // retransmitted INVITE for an existing call — ignore
        }
        let (req, src) = match &event {
            CallEvent::Sip { message, src } => match message.as_ref() {
                SipMessage::Request(r) => (r.clone(), *src),
                _ => return,
            },
            _ => return,
        };
        let call = build_initial_call(&req, src, &ctx.config, now_ms);
        ctx.state.create(call.clone());
        let result =
            handle_initial_invite(call.clone(), ctx.decision.as_ref(), ctx.limiter.as_ref(), &ctx.config, &ctx.id_gen, now_ms).await;
        crate::rules::invariants::enforce(&call, crate::rules::invariants::finalize(result))
    } else {
        // In-dialog: peek the in-memory map, falling back to the acting-backup
        // takeover read-path (S10b) — hydrate the call from the replica store's
        // backup partition when the primary crashed and the proxy failed this
        // dialog over to us. A genuine orphan (no replica) still rejects.
        let call = match ctx.state.hydrate_from_replica(&call_ref).await {
            Some((c, fresh)) => {
                // Failover timer re-arm: per-call timers (keepalive, global
                // duration, …) live in this node's in-memory `TimerService`, NOT
                // in the replicated call state — so a call freshly materialized
                // from a backup arrives with no live timers on THIS node. Re-arm
                // its serialized timer intents (`call.timers`, which IS
                // replicated) into the local driver, exactly once, on the
                // hydration that created it. Without this the hydrated call has
                // no keepalive (a dead peer is never probed) and no duration cap
                // (never reaped) → `b2bua_active_calls` leaks on the takeover
                // node — the failover analogue of the steady-state no-BYE leak.
                // `restore` past-due entries fire immediately (the keepalive then
                // re-arms itself on the next interval via the `keepalive` rule);
                // re-arming is idempotent — any subsequent rule-emitted
                // `ScheduleTimer` for the same id supersedes it via the driver's
                // epoch bump. Skipped for `fresh == false` (the call was already
                // resident and its timers are already live) to avoid double-arm.
                if fresh {
                    // Stamp this acting-backup takeover copy's activation instant
                    // so a later `Deactivate{as_of}` handback (ADR-0011 X11) can
                    // tell it predates the primary's reclaim.
                    ctx.state.mark_takeover(&call_ref);
                    ctx.timers.restore(c.timers.clone(), call_ref.clone()).await;
                }
                c
            }
            None => {
                maybe_reject_orphan(ctx, &event).await;
                // ORPHAN TEARDOWN (leak fix). This event was dispatched into a
                // per-call queue — one `bump_creation` (→ `b2bua_active_calls`) —
                // and `process` took the per-call lock above, but it resolved to
                // NO live call. Nothing will ever emit `RemoveCall`, and a per-call
                // dispatch worker exits ONLY on poison (its sender lives in the
                // queue map, so the channel never closes on its own). So the queue,
                // its idle task, the unmatched creation, and the lock entry would
                // ALL leak permanently — ~1 per orphan, which a mass-orphan failover
                // (thousands of in-dialog BYEs hitting a rebooted worker whose calls
                // were never reclaimed) turns into a multi-thousand `active_calls` +
                // `store_locks` ratchet that never drains. Release the lock entry
                // (no store mutation — `remove` would reverse-propagate a spurious
                // delete) and poison the queue so the worker exits and `removals`
                // balances `creations`. Drop our guard first so the poisoned worker
                // never contends on this call_ref's (now removed) lock.
                drop(_guard);
                ctx.state.discard_orphan(&call_ref);
                ctx.dispatcher.enqueue_poison(&call_ref);
                return;
            }
        };
        // The limiter-refresh timer is async (an HTTP call to migrate holds), so
        // it is handled outside the synchronous rule chain — like initial-INVITE.
        if matches!(
            &event,
            CallEvent::Timer {
                timer_type: TimerType::LimiterRefresh,
                ..
            }
        ) {
            let before = call.clone();
            let res = handle_limiter_refresh(ctx, call, now_ms).await;
            crate::rules::invariants::enforce(&before, crate::rules::invariants::finalize(res))
        } else {
            let rule_ctx = RuleContext {
                call: &call,
                call_ref: &call_ref,
                event: &event,
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
            execute_rules(&ctx.rules, &rule_ctx, &exec, default_handler)
        }
    };

    process_result(ctx, &call_ref, result, now_ms).await;
}

fn default_handler(ctx: &RuleContext) -> HandlerResult {
    HandlerResult::new(ctx.call.clone())
}

/// Handle a `LimiterRefresh` timer: migrate every live hold to the current
/// window (an async `/v1/refresh` call), update the stored windows, and re-arm
/// the timer while the call is alive. Port of `FrameworkLimiterRefresh.ts`.
async fn handle_limiter_refresh(ctx: &Arc<RouterCtx>, mut call: Call, now_ms: i64) -> HandlerResult {
    use crate::limiter::LimiterHold;

    let holds: Vec<LimiterHold> = call
        .limiter_entries
        .iter()
        .filter(|e| e.increment_succeeded != Some(false))
        .map(|e| LimiterHold {
            limiter_id: e.limiter_id.clone(),
            window: e.origin_window,
        })
        .collect();

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
            if !req.method.eq_ignore_ascii_case("ACK") {
                let resp = generate_response(
                    req,
                    481,
                    "Call/Transaction Does Not Exist",
                    &GenerateResponseOpts::default(),
                );
                ctx.txn.send_response(resp, *src).await;
            }
        }
    }
}

/// Interpret a handler result: persist → critical → outbound → soft → buffered.
async fn process_result(ctx: &Arc<RouterCtx>, call_ref: &str, result: HandlerResult, now_ms: i64) {
    // Persist first (the source's invariant: state lands before effects run).
    ctx.state.update(result.call.clone());

    // Replicate an active, backed-up call to its peer after each authoritative
    // mutation (the S10 flush-on-mutation wiring point — `replication.rs` defers
    // sourcing the backup peer to S10, which the cookie-stamped `topology.bak`
    // now provides). `CallState::flush` is a no-op for calls with no replicable
    // topology, so the non-HA path is unchanged; for a backed-up call it routes
    // through the S8 write-side policy (Forward when primary, Reverse when
    // acting-backup) so the backup holds the latest `call_gen`. The flush rides
    // the buffered terminate-writer (non-blocking). Terminated calls take the
    // `remove` path instead (propagates a delete), so gate on the active state.
    if result.call.state == CallModelState::Active
        && result
            .call
            .topology
            .as_ref()
            .is_some_and(|t| !t.bak.is_empty())
    {
        ctx.state.flush(&result.call);
    }

    for eff in &result.effects.critical {
        match eff {
            CriticalStateEffect::ScheduleTimer(entry) => {
                ctx.timers.schedule(entry.clone(), call_ref.to_string()).await;
            }
            CriticalStateEffect::CancelTimer { id } => {
                ctx.timers.cancel(call_ref.to_string(), id.clone()).await
            }
            CriticalStateEffect::CancelAllTimers => {
                ctx.timers.cancel_all(call_ref.to_string()).await
            }
            CriticalStateEffect::Flush => ctx.state.flush(&result.call),
            CriticalStateEffect::RemoveCall => {
                ctx.state.remove(call_ref);
                ctx.txn.cancel_txns_for_call(call_ref).await;
                // Poison the per-call dispatch queue; its worker exits and bumps
                // `removal` exactly once (dispatch.rs). We deliberately do NOT
                // bump here — removal is counted at the single dispatch-queue
                // teardown site so creations/removals stay a matched pair (one
                // per call_ref). Counting here too double-counted every removal
                // (~2× creations) and made `active_calls` saturate to 0.
                ctx.dispatcher.enqueue_poison(call_ref);
            }
        }
    }

    for eff in &result.effects.outbound {
        let dest: SocketAddr = match format!("{}:{}", eff.destination.0, eff.destination.1).parse() {
            Ok(d) => d,
            Err(_) => continue,
        };
        match (&eff.body, &eff.mode) {
            (OutboundBody::Response(resp), _) => ctx.txn.send_response(resp.clone(), dest).await,
            (OutboundBody::Request(req), OutboundTxnMode::NewClient(kind)) => {
                let _ = ctx.txn.send_request(req.clone(), dest, *kind).await;
            }
            (OutboundBody::Request(req), OutboundTxnMode::Raw) => {
                ctx.txn.send_raw(serialize(&SipMessage::Request(req.clone())), dest).await
            }
            (OutboundBody::Request(req), OutboundTxnMode::ServerResponse) => {
                // A request tagged ServerResponse is a misuse; send raw as a fallback.
                ctx.txn.send_raw(serialize(&SipMessage::Request(req.clone())), dest).await
            }
        }
    }

    for eff in &result.effects.soft {
        match eff {
            SoftBoundedEffect::DecrementLimiter { limiter_id, window } => {
                ctx.limiter
                    .release(&[crate::limiter::LimiterHold {
                        limiter_id: limiter_id.clone(),
                        window: *window,
                    }])
                    .await
            }
        }
    }

    for eff in &result.effects.buffered {
        match eff {
            BufferedObservabilityEffect::WriteCdr => ctx.cdr.write(&result.call, now_ms).await,
        }
    }

    // Fire-and-forget: detached async work that folds its result back into the
    // call via a re-entrant internal event (the REFER `/call/refer` round-trip,
    // and the generic re-enter path).
    for eff in result.effects.fire_and_forget {
        match eff {
            FireAndForgetEffect::ReferAsyncHttp { call_ref, request } => {
                let ctx2 = ctx.clone();
                tokio::spawn(async move {
                    // Deserialize the request the seed rule built (mirrors the
                    // TS POST body); call the decision backend; map to a
                    // `refer-http-result` internal event; re-enter the chain.
                    let req = parse_call_refer_request(&request);
                    let (outcome, payload) = match ctx2.decision.call_refer(req).await {
                        Ok(CallReferResponse::Allow {
                            destination,
                            new_refer_to,
                            update_headers,
                            no_answer_timeout_sec,
                            callback_context,
                        }) => {
                            let mut p = serde_json::Map::new();
                            p.insert("action".into(), serde_json::json!("allow"));
                            p.insert(
                                "destination".into(),
                                serde_json::json!({
                                    "host": destination.host,
                                    "port": destination.port,
                                    "transport": destination.transport,
                                }),
                            );
                            if let Some(v) = new_refer_to {
                                p.insert("new_refer_to".into(), serde_json::json!(v));
                            }
                            if let Some(v) = update_headers {
                                p.insert("update_headers".into(), serde_json::json!(v));
                            }
                            if let Some(v) = no_answer_timeout_sec {
                                p.insert("no_answer_timeout_sec".into(), serde_json::json!(v));
                            }
                            if let Some(v) = callback_context {
                                p.insert("callback_context".into(), serde_json::json!(v));
                            }
                            ("allow", serde_json::Value::Object(p))
                        }
                        Ok(CallReferResponse::Reject { code, reason }) => (
                            "reject",
                            serde_json::json!({ "reject_code": code, "reject_reason": reason }),
                        ),
                        Err(_) => ("error", serde_json::json!({})),
                    };
                    let ev = CallEvent::InternalEvent {
                        call_ref,
                        topic: "refer-http-result".to_string(),
                        outcome: outcome.to_string(),
                        payload,
                    };
                    // Re-enter via the router's event channel rather than
                    // calling `on_event` directly: the `on_event → process →
                    // process_result → on_event` cycle has an opaque future type
                    // the compiler cannot prove `Send`. Routing the event back
                    // through `run`'s loop keeps re-entry single-threaded and
                    // breaks the recursion.
                    let _ = ctx2.reentry_tx.send(ev);
                });
            }
            FireAndForgetEffect::FailureAsyncHttp { call_ref, request } => {
                let ctx2 = ctx.clone();
                tokio::spawn(async move {
                    // The seed rule's request JSON carries the failure context
                    // plus `failed_leg_id` (echoed back so the resolution rule
                    // can cancel the right no-answer timer / relay the failure).
                    let req = parse_call_failure_request(&request);
                    let failed_leg_id = request
                        .get("failed_leg_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let (outcome, payload) = match ctx2.decision.call_failure(req).await {
                        Ok(crate::decision::CallFailureResponse::Failover(route)) => {
                            let mut p = serde_json::Map::new();
                            p.insert(
                                "destination".into(),
                                serde_json::json!({
                                    "host": route.destination.host,
                                    "port": route.destination.port,
                                }),
                            );
                            if let Some(v) = route.new_ruri {
                                p.insert("new_ruri".into(), serde_json::json!(v));
                            }
                            if let Some(v) = route.update_headers {
                                p.insert("update_headers".into(), serde_json::json!(v));
                            }
                            if let Some(v) = route.no_answer_timeout_sec {
                                p.insert("no_answer_timeout_sec".into(), serde_json::json!(v));
                            }
                            if let Some(v) = route.callback_context {
                                p.insert("callback_context".into(), serde_json::json!(v));
                            }
                            p.insert("failed_leg_id".into(), serde_json::json!(failed_leg_id));
                            ("failover", serde_json::Value::Object(p))
                        }
                        // Terminate / backend error → relay the original failure
                        // (response path) + tear the call down. Echo the failure's
                        // status/reason the seed stashed for the relay.
                        _ => {
                            let mut p = serde_json::Map::new();
                            if let Some(v) = request.get("sip_code") {
                                p.insert("status".into(), v.clone());
                            }
                            if let Some(v) = request.get("sip_reason") {
                                p.insert("reason".into(), v.clone());
                            }
                            p.insert("failed_leg_id".into(), serde_json::json!(failed_leg_id));
                            ("terminate", serde_json::Value::Object(p))
                        }
                    };
                    let ev = CallEvent::InternalEvent {
                        call_ref,
                        topic: "call-failure-result".to_string(),
                        outcome: outcome.to_string(),
                        payload,
                    };
                    let _ = ctx2.reentry_tx.send(ev);
                });
            }
            FireAndForgetEffect::Reenter(ev) => {
                let _ = ctx.reentry_tx.send(*ev);
            }
        }
    }
}

/// Rebuild a [`CallReferRequest`] from the JSON the seed rule emitted.
fn parse_call_refer_request(v: &serde_json::Value) -> crate::decision::CallReferRequest {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let sip_headers = v
        .get("sip_headers")
        .and_then(|x| x.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    crate::decision::CallReferRequest {
        call_id: s("call_id").unwrap_or_default(),
        dialog_id: s("dialog_id").unwrap_or_default(),
        callback_context: s("callback_context"),
        refer_to: s("refer_to").unwrap_or_default(),
        referred_by: s("referred_by"),
        sip_headers,
    }
}

/// Rebuild a [`CallFailureRequest`] from the JSON the seed rule emitted.
fn parse_call_failure_request(v: &serde_json::Value) -> crate::decision::CallFailureRequest {
    crate::decision::CallFailureRequest {
        callback_context: v
            .get("callback_context")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        failure: crate::decision::FailureInfo {
            origin: v
                .get("origin")
                .and_then(|x| x.as_str())
                .unwrap_or("external")
                .to_string(),
            status_code: v
                .get("sip_code")
                .and_then(|x| x.as_u64())
                .map(|c| c as u16),
            limiter_id: v.get("limiter_id").and_then(|x| x.as_str()).map(str::to_string),
        },
    }
}

/// Extract `(cr, lg)` from a Via header value's `;cr=`/`;lg=` params.
fn via_cr_lg(via: Option<&str>) -> Option<(Option<String>, String)> {
    let via = via?;
    if !via.contains("cr=") && !via.contains("lg=") {
        return None;
    }
    let mut cr = None;
    let mut lg = "a".to_string();
    for part in via.split(';').skip(1) {
        let (k, v) = part.split_once('=').unwrap_or((part.trim(), ""));
        match k.trim() {
            "cr" => cr = Some(crate::stack_identity::decode_param(v.trim())),
            "lg" => lg = crate::stack_identity::decode_param(v.trim()),
            _ => {}
        }
    }
    Some((cr, lg))
}
