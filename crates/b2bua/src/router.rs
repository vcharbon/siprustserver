//! `SipRouter` — consumes the transaction-layer event stream + the timer fire
//! channel, resolves each event's `callRef` (synchronously), dispatches the
//! handler body to the per-call FIFO, and interprets the typed effects in the
//! fixed order (persist → critical → outbound → soft → buffered). Port of the
//! load-bearing half of `SipRouter.ts` (`routeKey` + `withCall` + `processResult`).

use std::net::SocketAddr;
use std::sync::Arc;

use call::{CallModelState, Direction};
use sip_clock::Clock;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::message_helpers::parse_uri_params;
use sip_message::{serialize, SipMessage};
use sip_txn::{IdGen, TransactionLayer};
use tokio::sync::mpsc;

use crate::cdr::CdrWriter;
use crate::config::B2buaConfig;
use crate::decision::CallDecisionEngine;
use crate::dispatch::PerCallDispatcher;
use crate::effects::{
    BufferedObservabilityEffect, CriticalStateEffect, HandlerResult, OutboundBody, OutboundTxnMode,
    SoftBoundedEffect,
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
    mut timer_rx: mpsc::Receiver<CallEvent>,
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
        }
    }
}

async fn on_event(ctx: &Arc<RouterCtx>, event: CallEvent) {
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

    let res = resolve(ctx, &event);
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
            Some(c) => c,
            None => {
                maybe_reject_orphan(ctx, &event).await;
                return;
            }
        };
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
    };

    process_result(ctx, &call_ref, result, now_ms).await;
}

fn default_handler(ctx: &RuleContext) -> HandlerResult {
    HandlerResult::new(ctx.call.clone())
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
async fn process_result(ctx: &RouterCtx, call_ref: &str, result: HandlerResult, now_ms: i64) {
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
            CriticalStateEffect::CancelTimer { id } => ctx.timers.cancel(id.clone()).await,
            CriticalStateEffect::CancelAllTimers => {
                ctx.timers.cancel_all(call_ref.to_string()).await
            }
            CriticalStateEffect::Flush => ctx.state.flush(&result.call),
            CriticalStateEffect::RemoveCall => {
                ctx.state.remove(call_ref);
                ctx.txn.cancel_txns_for_call(call_ref).await;
                ctx.dispatcher.enqueue_poison(call_ref);
                ctx.metrics.bump_removal();
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
                ctx.limiter.decrement(limiter_id, *window).await
            }
        }
    }

    for eff in &result.effects.buffered {
        match eff {
            BufferedObservabilityEffect::WriteCdr => ctx.cdr.write(&result.call, now_ms).await,
        }
    }

    // Fire-and-forget (refer/re-entry) is deferred with its consumers.
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
