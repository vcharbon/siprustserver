//! `SipRouter` — consumes the transaction-layer event stream + the timer fire
//! channel, resolves each event's `callRef` (synchronously), dispatches the
//! handler body to the per-call FIFO, and interprets the typed effects in the
//! fixed order (persist → critical → outbound → soft → buffered).
//!
//! One concern per submodule: [`resolve`] keys events to calls, [`ingress`] is
//! the pre-dispatch pipeline, [`process`] the per-call handler body,
//! [`interpret`] the effect interpreter, [`callouts`] the fire-and-forget
//! async-HTTP folds, [`reclaim`] the replication reclaim/discharge funnels,
//! [`restore_hygiene`] the replicated-timer restore seam, [`release`] the one
//! per-call teardown executor, [`responses`] the locally-authored response
//! builders, and [`peer_metrics`] per-peer failure attribution.

mod callouts;
mod ingress;
mod interpret;
mod peer_metrics;
mod process;
mod reclaim;
mod release;
mod resolve;
mod responses;
mod restore_hygiene;

pub(crate) use reclaim::reap_expired_replicas;
// Consumed by the S7 readiness tests (`repl::s7_tests`) only.
#[cfg(test)]
pub(crate) use responses::build_options_health_response;

use std::net::SocketAddr;
use std::sync::Arc;

use sip_clock::Clock;
use sip_txn::{IdGen, TransactionLayer};
use tokio::sync::mpsc;

use crate::cdr::CdrWriter;
use crate::config::B2buaConfig;
use crate::decision::CallDecisionEngine;
use crate::dispatch::PerCallDispatcher;
use crate::event::CallEvent;
use crate::limiter::CallLimiter;
use crate::metrics::B2buaMetrics;
use crate::obligations::ObligationSet;
use crate::overload::OverloadSignal;
use crate::repl::Readiness;
use crate::rules::{RuleDefinition, ServiceDef};
use crate::store::{CallState, StoreFaults};
use crate::timers::TimerService;

/// Host-injected capability for the generic service-authorable async-HTTP
/// callback ([`RuleAction::ServiceHttpRequest`](crate::rules::RuleAction)). A
/// logical [`endpoint`](crate::rules::RuleAction) path is mapped onto `base`;
/// the same `http_net::HttpTransport` seam the limiter rides carries the request
/// (real `reqwest` in the runner, the simulated fabric in tests). The caller
/// owns the fail-safe budget: `default_timeout` bounds any request that omits a
/// per-request `timeout_ms`. Mirrors [`limiter_http`](crate::limiter_http)'s
/// transport+addr+timeout shape. `None` on [`B2buaDeps`](crate::B2buaDeps) →
/// today's behaviour (a service that fires the effect still gets an
/// `outcome:"error"` re-entry, never a stranded machine).
pub struct AdaptationHttpPort {
    /// The pluggable HTTP transport (binary-safe: `HttpRequest`/`HttpResponse`
    /// bodies are `Vec<u8>`).
    pub transport: Arc<dyn http_net::HttpTransport>,
    /// Base address the logical endpoint path is dialed against.
    pub base: SocketAddr,
    /// Fail-safe per-request budget applied when the effect omits `timeout_ms`.
    pub default_timeout: std::time::Duration,
}

/// Everything a handler body + the interpreter need. Shared via `Arc`.
pub struct RouterCtx {
    pub config: B2buaConfig,
    pub state: CallState,
    /// Live-path store-fault probe (ADR-0023). Consulted BEFORE the sync map
    /// reads at the three live lookup sites in `process`; default = no
    /// faults (a no-op atomic read). See `store::faults` for the defined
    /// degraded-mode semantics.
    pub store_faults: StoreFaults,
    pub txn: TransactionLayer,
    pub timers: TimerService,
    pub dispatcher: PerCallDispatcher,
    pub decision: Arc<dyn CallDecisionEngine>,
    pub limiter: Arc<dyn CallLimiter>,
    pub cdr: Arc<dyn CdrWriter>,
    pub id_gen: Arc<IdGen>,
    pub clock: Clock,
    /// The composed engine rule list — `flatten(services.rules) ++ core_rules()`
    /// (ADR-0016). Equal to `default_rules()` while no service is registered.
    pub rules: Arc<Vec<RuleDefinition>>,
    /// Registered callflow services (ADR-0016). Their `init` hooks run at call
    /// setup; their rules are already flattened into `rules`. Empty until a
    /// service is retrofitted.
    pub services: Arc<Vec<ServiceDef>>,
    pub metrics: B2buaMetrics,
    /// The obligation registry (ADR-0020 X7) — what every call owes at release
    /// (the CDR, the limiter decrements), derived from the snapshot by
    /// `invariants::enforce` on each `→ Terminated` transition.
    pub obligations: Arc<ObligationSet>,
    /// Self-reported readiness driving the OPTIONS health responder (S7). The
    /// default/legacy path uses [`Readiness::always_ready`] → always 200.
    pub readiness: Readiness,
    /// Worker-side overload signal stamped on every OPTIONS-200 reply as
    /// `X-Overload: v=1; elu=…; gc=…; adm=…`. The front proxy's ELU-band AIMD
    /// (`sip_proxy::load_observer`) consumes it; the EWMAs advance only while a
    /// sampler task drives [`OverloadSignal::sample`].
    pub overload: OverloadSignal,
    /// Re-entrant event sink: fire-and-forget work (the async `/call/refer`
    /// round-trip) folds its result back into the router by sending a
    /// `CallEvent::InternalEvent` here, which `run` consumes via `on_event` —
    /// keeping re-entry single-threaded and out of a non-`Send` async cycle.
    pub reentry_tx: mpsc::UnboundedSender<CallEvent>,
    /// Host-injected generic async-HTTP capability (ADR-0016 seam). `Arc`-shared
    /// into every per-call `ctx.clone()` exactly like `decision`/`limiter`;
    /// `None` reproduces today's behaviour (the `ServiceHttpRequest` dispatch
    /// arm then folds an `outcome:"error"` re-entry instead of hitting a wire).
    pub adaptation_http: Option<Arc<AdaptationHttpPort>>,
}

/// Replication-driven commands the puller/supervisor inject into the router loop
/// (ADR-0011 X11 / ADR-0014 fail-back). Routed through the same single-threaded
/// `run` loop as SIP events so reclaim never races the per-call handlers.
#[derive(Debug, Clone)]
pub enum ReplCommand {
    /// **Bulk reclaim** — materialise every `pri:{self}` call into the
    /// live map + re-arm timers (a rebooted primary re-*serving* its partition,
    /// not just re-storing it). Fired once the supervisor reports bootstrap-complete.
    /// Keepalive timers are *smoothed* (oldest-overdue first; see
    /// `reclaim::reclaim_all`) so a freshly-rehydrated node is not flooded by a
    /// burst of past-due OPTIONS.
    ReclaimAll,
    /// **Reactive reclaim** of one call a backup just reverse-flushed to us — the
    /// flip-race straggler an acting-backup took over *after* the bulk sweep.
    ReclaimCall(String),
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
                Some(ev) => ingress::on_event(&ctx, CallEvent::from_txn(ev)).await,
                None => break,
            },
            ev = timer_rx.recv() => {
                if let Some(ev) = ev {
                    ingress::on_event(&ctx, ev).await;
                }
            },
            ev = reentry_rx.recv() => {
                if let Some(ev) = ev {
                    ingress::on_event(&ctx, ev).await;
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

/// Interpret a replication-driven [`ReplCommand`] (ADR-0011 X11 / ADR-0014).
async fn on_repl_command(ctx: &Arc<RouterCtx>, cmd: ReplCommand) {
    match cmd {
        ReplCommand::ReclaimAll => reclaim::reclaim_all(ctx).await,
        ReplCommand::ReclaimCall(call_ref) => reclaim::reconcile_reverse_flush(ctx, &call_ref).await,
    }
}
