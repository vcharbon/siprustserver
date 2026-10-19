//! `SipRouter` — consumes the transaction-layer event stream + the timer fire
//! channel, resolves each event's `callRef` (synchronously), dispatches the
//! handler body to the per-call FIFO, and interprets the typed effects in the
//! fixed order (persist → critical → outbound → soft → buffered).
//!
//! One concern per submodule: `resolve` keys events to calls, `ingress` is
//! the pre-dispatch pipeline, `admit` a new INVITE's router rungs of the
//! admission ladder, `process` the per-call handler body,
//! `interpret` the effect interpreter, `callouts` the fire-and-forget
//! async-HTTP folds, `materialise` the one path a stored replica takes to
//! become the live copy this node serves (takeover or reclaim), `departure`
//! the takeover of a departed peer's in-flight calls, `undecided` the end of
//! a call materialised before its routing decision, `quiesced` the
//! self-release turn of a takeover copy, `stand_down` the turn that releases a
//! takeover copy on its primary's write, `dropped_on_delete` the turn that
//! drops a live copy of a call its backup discharged, `reclaim`
//! the replication reclaim/discharge funnels,
//! `restore_hygiene` the replicated-timer restore seam, `release` the one
//! per-call teardown executor, `responses` the locally-authored response
//! builders, `peer_metrics` per-peer failure attribution, `turn` the
//! item offered to a call's queue, `owed` what a request whose body never
//! ran is owed, and `unroutable` the answer and accounting of an event
//! naming no call.

mod admit;
#[cfg(test)]
mod answer_deadline_tests;
mod callout_requests;
mod callouts;
#[cfg(test)]
mod cancel_crossing_tests;
mod departure;
mod dialog_gate;
#[cfg(test)]
mod dialog_gate_tests;
mod dropped_on_delete;
mod fold_payload;
mod incarnation_gate;
mod ingress;
mod initial_turn;
mod interpret;
pub(crate) mod late_prack;
mod materialise;
mod notes;
mod owed;
mod peer_metrics;
pub(crate) use peer_metrics::outbound_proxy_target;
mod process;
#[cfg(test)]
mod provisional_turn_tests;
mod quiesced;
mod reclaim;
mod refer_result;
mod release;
mod resolve;
#[cfg(test)]
mod resolve_tests;
#[cfg(test)]
mod response_facts_tests;
mod responses;
mod restore_hygiene;
mod route_payload;
mod stand_down;
mod store_fault_gate;
mod stray_cancel;
#[cfg(test)]
pub(crate) mod test_support;
mod turn;
mod turn_label;
pub(crate) mod txn_event;
mod unaddressable;
mod undecided;
mod unrecoverable;
mod unroutable;

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
use crate::limiter::LimiterWorker;
use crate::metrics::B2buaMetrics;
use crate::obligations::ObligationSet;
use crate::overload::OverloadSignal;
use crate::repl::Readiness;
use crate::rules::{RuleDefinition, ServiceDef};
use crate::store::{CallState, StoreFaults};
use crate::timers::TimerService;
use b2bua_sdk::event::CallEvent;

/// Host-injected capability for the generic service-authorable async-HTTP
/// callback ([`RuleAction::ServiceHttpRequest`](crate::rules::RuleAction)). A
/// logical [`endpoint`](crate::rules::RuleAction) path is sent to the backend
/// `client` names, under the generic retry rule (ADR-0043): one attempt has
/// `attempt`, every attempt together the request's budget, its `timeout_ms` or
/// `default_timeout`. `default_timeout` also sets the answer deadline of a
/// request that states no `timeout_ms` (ADR-0039). `None` on
/// [`B2buaDeps`](crate::B2buaDeps) → a service that fires the effect gets an
/// `outcome:"error"` re-entry.
#[derive(Clone)]
pub struct AdaptationHttpPort {
    /// The backend every request goes to (binary-safe: `HttpRequest`/
    /// `HttpResponse` bodies are `Vec<u8>`).
    pub client: http_net::BackendClient,
    /// One attempt's budget, at most the request's.
    pub attempt: std::time::Duration,
    /// A request's budget, every attempt together, when the effect omits
    /// `timeout_ms`.
    pub default_timeout: std::time::Duration,
}

impl AdaptationHttpPort {
    /// A port to the backend at `base` over `transport`, with the default
    /// client settings and a breaker probing `/`; one attempt fills a
    /// request's budget, `default_timeout` when the effect states none.
    pub fn addressed(
        transport: Arc<dyn http_net::HttpTransport>,
        base: SocketAddr,
        default_timeout: std::time::Duration,
    ) -> Self {
        let client = http_net::BackendClient::new(
            transport,
            Arc::new(http_net::ResolvedTarget::addr(base)),
            http_net::ClientSettings::default(),
            http_net::BreakerConfig::probing("/"),
        );
        Self { client, attempt: default_timeout, default_timeout }
    }
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
    /// Wire-fault seam (`wire_faults`): read by the action executor at the one
    /// emission site it guards; default = never armed.
    pub wire_faults: crate::wire_faults::WireFaults,
    pub txn: TransactionLayer,
    /// The router loop's pulse: stamped per turn, and per call by a turn that
    /// works through many (the bulk reboot reclaim).
    pub router_pulse: Arc<sip_txn::LoopPulse>,
    pub timers: TimerService,
    pub(crate) dispatcher: PerCallDispatcher<turn::Turn>,
    /// Hears a call past its lifetime cap or at its overflow ceiling, as the
    /// dispatcher's offers state them.
    pub reaper: crate::reaper::Reaper,
    pub decision: Arc<dyn CallDecisionEngine>,
    /// The worker's one handle on its call limiter: every admit, release
    /// and refresh, none of which a call waits on beyond the admit bound.
    pub limiter: LimiterWorker,
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
    /// (the CDR, the limiter release), derived from the snapshot by
    /// `invariants::enforce` on each `→ Terminated` transition.
    pub obligations: Arc<ObligationSet>,
    /// Self-reported readiness driving the OPTIONS health responder. The
    /// unwired node uses [`Readiness::always_ready`] → always 200.
    pub readiness: Readiness,
    /// The To-tag of the OPTIONS health answer, derived from the request: the
    /// transaction layer holds no transaction for an out-of-dialog OPTIONS.
    pub options_tagger: sip_txn::StatelessTagger,
    /// Whether a call's primary has departed (ADR-0047). `None` on an unwired
    /// node: no call there has another primary.
    pub departed: Option<crate::repl::DepartedProbe>,
    /// Worker-side overload signal stamped on every OPTIONS-200 reply as
    /// `X-Overload: v=1; elu=…; gc=…; adm=…`. The front proxy's ELU-band AIMD
    /// (`sip_proxy::load_observer`) consumes it; the EWMAs advance only while a
    /// sampler task drives [`OverloadSignal::sample`]. The panic-ELU and CPS
    /// bucket rungs read it; the run loop is the bucket's only taker.
    pub overload: OverloadSignal,
    /// Memory admission gate (ADR-0037): the capacity rung's ceilings and
    /// RSS sample. The overload sampler task samples it.
    pub capacity: crate::capacity::CapacityGate,
    /// The worker's refusals of new INVITEs: the one answer every admission
    /// rung sends, and the memo the brake and the transaction layer share.
    pub refusals: crate::admission::Refusals,
    /// New calls admitted at ingress and not born yet: the capacity rung
    /// counts them as calls.
    pub(crate) unborn: admit::Unborn,
    /// Re-entrant event sink: fire-and-forget work folds its result back into
    /// the router by sending a `CallEvent::InternalEvent` here, which `run`
    /// consumes via `on_event` — keeping re-entry single-threaded and out of a
    /// non-`Send` async cycle. A sender sends only a reaper verdict or one
    /// answer per request the call sent: every internal event waits past all
    /// of the call's dispatch bounds (`dispatch::class`).
    pub reentry_tx: mpsc::UnboundedSender<CallEvent>,
    /// Keepalive-timeout burst aggregation keyed by the failed leg's egress hop
    /// (ADR-0026): a peer going away is ONE episode — rising edge, ~5 s
    /// summaries, falling-edge totals — not one line per dead call.
    pub keepalive_waves: Arc<observe::WaveSet>,
    /// The configured outbound proxy, its name looked up off the router task
    /// (per-peer failure attribution reads only its kept addresses).
    pub outbound_proxy: Option<Arc<http_net::ResolvedTarget>>,
    /// Events that resolved to no call, aggregated per class (ADR-0026).
    pub unroutable_waves: crate::lifecycle::UnroutableWaves,
    /// The PRACKs released calls still answer 200 (RFC 3262 §3).
    pub(crate) late_pracks: late_prack::LatePrackBook,
    /// Host-injected generic async-HTTP capability (ADR-0016 seam). `Arc`-shared
    /// into every per-call `ctx.clone()` exactly like `decision`/`limiter`;
    /// `None` → the `ServiceHttpRequest` dispatch arm folds an `outcome:"error"`
    /// re-entry instead of hitting a wire.
    pub adaptation_http: Option<Arc<AdaptationHttpPort>>,
}

impl RouterCtx {
    /// The per-process values a turn reads (ADR-0052 §2).
    pub(crate) fn framework(&self) -> crate::rule_chain::Framework<'_> {
        crate::rule_chain::Framework {
            config: &self.config,
            rules: &self.rules,
            obligations: &self.obligations,
        }
    }
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
    /// A backup's reverse flush the store's `(p,b)` gate refused: the body is
    /// handed up so the call model can fold lifecycle progress the vector
    /// cannot see (`reclaim::fold_refused_flush`). `origin_now_ms` is
    /// the flushing node's wall clock, for timer re-anchoring.
    ReverseFlushRefused { call_ref: String, body: Arc<[u8]>, origin_now_ms: i64 },
    /// A primary's forward flush this backup refused (ADR-0031 D3): the body is
    /// handed up on the same terms as the reverse case, and folds only where a
    /// live takeover copy of the call exists — an Element alone is kept as it
    /// is, its merge deferred to the next materialisation. Only the steady tail
    /// sends it: a bulk re-seed's refusals are the state of the two views, not
    /// events, and are counted alone.
    ForwardFlushRefused { call_ref: String, body: Arc<[u8]>, origin_now_ms: i64 },
    /// A primary's forward write this backup applied to the Element of a call
    /// it serves as a live takeover copy, in a scan or on the tail: handed up so
    /// the copy can stand down on it (`stand_down`, ADR-0047 D7).
    ForwardFlushApplied { call_ref: String, write: PrimaryWrite },
    /// A backup's delete of one of this node's calls, applied on the reclaim
    /// stream with the incarnation it names: the backup's takeover copy
    /// discharged the call, and a live copy of it here is dropped
    /// (`dropped_on_delete`, ADR-0047 D8).
    ReverseDeleteApplied { call_ref: String, incarnation: Option<String> },
    /// The peer of this ordinal left the membership set while its flows ran: a
    /// real stop. Its calls this node backs up with an INVITE transaction to
    /// serve are taken over at once (`departure`, ADR-0047). Sent once per stop.
    PeerDeparted(String),
}

/// A write of a call's primary, as a backup applied it to its Element.
#[derive(Debug, Clone)]
pub enum PrimaryWrite {
    /// The primary's body of the call.
    Put(Arc<[u8]>),
    /// The primary ended the call and deleted it; the incarnation the delete
    /// names, when it names one.
    Delete(Option<String>),
}

/// What one turn of the router took.
enum Work {
    Event(CallEvent),
    Repl(ReplCommand),
}

/// Run the router loop over the txn-event + timer-fire channels until both close.
/// `repl_rx` is the fail-back command receiver of a wired node; an unwired node
/// passes `None` and the loop never polls for one. Each turn is stamped on
/// `ctx.router_pulse` from the instant it takes work until it waits again.
pub async fn run(
    ctx: Arc<RouterCtx>,
    mut txn_rx: mpsc::Receiver<sip_txn::TransactionEvent>,
    mut timer_rx: mpsc::UnboundedReceiver<CallEvent>,
    mut reentry_rx: mpsc::UnboundedReceiver<CallEvent>,
    mut repl_rx: Option<mpsc::UnboundedReceiver<ReplCommand>>,
) {
    let pulse = ctx.router_pulse.clone();
    // A timer or re-entry channel whose senders are all gone retires its arm
    // instead of resolving `None` on every poll.
    let (mut timers_open, mut reentry_open) = (true, true);
    loop {
        let work = tokio::select! {
            ev = txn_rx.recv() => match ev {
                Some(ev) => Work::Event(txn_event::call_event(ev)),
                None => break,
            },
            ev = timer_rx.recv(), if timers_open => match ev {
                Some(ev) => Work::Event(ev),
                None => {
                    timers_open = false;
                    continue;
                }
            },
            ev = reentry_rx.recv(), if reentry_open => match ev {
                Some(ev) => Work::Event(ev),
                None => {
                    reentry_open = false;
                    continue;
                }
            },
            cmd = next_fail_back(&mut repl_rx) => match cmd {
                Some(cmd) => Work::Repl(cmd),
                // Retires the arm if every sender is gone, instead of resolving
                // `None` on every poll; no current wiring reaches it (the
                // supervisor the router's readiness holds keeps a sender).
                None => {
                    repl_rx = None;
                    continue;
                }
            },
        };
        pulse.turning();
        match work {
            Work::Event(ev) => ingress::on_event(&ctx, ev).await,
            Work::Repl(cmd) => on_repl_command(&ctx, cmd).await,
        }
        pulse.waiting();
    }
}

/// The next fail-back command; pending forever when no receiver exists.
async fn next_fail_back(
    rx: &mut Option<mpsc::UnboundedReceiver<ReplCommand>>,
) -> Option<ReplCommand> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Interpret a replication-driven [`ReplCommand`] (ADR-0011 X11 / ADR-0014).
async fn on_repl_command(ctx: &Arc<RouterCtx>, cmd: ReplCommand) {
    match cmd {
        ReplCommand::ReclaimAll => reclaim::reclaim_all(ctx).await,
        ReplCommand::ReclaimCall(call_ref) => {
            reclaim::reconcile_reverse_flush(ctx, &call_ref).await
        }
        ReplCommand::ReverseFlushRefused { call_ref, body, origin_now_ms } => {
            let dir = reclaim::FlushDirection::Reverse;
            reclaim::fold_refused_flush(ctx, dir, &call_ref, &body, origin_now_ms).await
        }
        ReplCommand::ForwardFlushRefused { call_ref, body, origin_now_ms } => {
            let dir = reclaim::FlushDirection::Forward;
            reclaim::fold_refused_flush(ctx, dir, &call_ref, &body, origin_now_ms).await
        }
        ReplCommand::ForwardFlushApplied { call_ref, write } => {
            stand_down::offer(ctx, call_ref, write).await
        }
        ReplCommand::ReverseDeleteApplied { call_ref, incarnation } => {
            dropped_on_delete::offer(ctx, call_ref, incarnation).await
        }
        ReplCommand::PeerDeparted(ordinal) => departure::take_over_departed(ctx, &ordinal),
    }
}
