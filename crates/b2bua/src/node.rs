//! `B2buaCore` — composes the dispatcher + router + call store + transaction
//! layer + timer service + decision engine + CDR writer over a bound UDP
//! endpoint, and spawns the router loop. Construct it over an endpoint (in
//! tests, `Harness::bind_sut`), then drive SIP at the endpoint's address.

use std::sync::Arc;
use std::time::Duration;

use observe::{spawn_keeping_node, NodeScope};
use sip_message::parser::custom::CustomParser;
use sip_message::SipParser;
use sip_net::UdpEndpoint;
use sip_txn::TransactionLayer;

use crate::admission::Refusals;
use crate::capacity::CapacityGate;
use crate::cdr::CdrWriter;
use crate::dispatch::PerCallDispatcher;
use crate::metrics::B2buaMetrics;
use crate::overload::OverloadSignal;
use crate::repl::{Readiness, ReplServer, ReplicatingCallStore, ReplicationSupervisor, RouterSink};
use crate::router::{self, RouterCtx};
use crate::rules::{compose_rules, default_rules_with, ServiceDef};
use crate::store::CallState;
use crate::timers::TimerService;

mod deps;
pub mod replica;
pub use deps::{B2buaDeps, ReplicationSetup};

mod drain;
#[cfg(test)]
mod drain_tests;
mod fence;
use fence::Fence;
mod introspection;
#[cfg(test)]
mod loops_tests;
mod readiness;
mod release_all;
mod samplers;
mod txn_config;
use txn_config::txn_config;
#[cfg(test)]
mod unwired_tests;

/// A running B2BUA worker. Holds the shared context; the router loop runs on a
/// spawned task that lives until the endpoint closes.
pub struct B2buaCore {
    ctx: Arc<RouterCtx>,
    metrics: B2buaMetrics,
    cdr: Arc<dyn CdrWriter>,
    /// The worker's refusals of new INVITEs; the transaction layer holds
    /// their memo.
    refusals: Refusals,
    /// Readiness handle (the supervisor-backed one when replication is wired,
    /// else the always-ready one of an unwired node). Kept so
    /// [`begin_draining`] can latch it.
    readiness: Readiness,
    /// Worker-side overload signal. Re-exposed via
    /// [`overload`](Self::overload) so callers/tests can read the published
    /// header and advance the `adm` counter; a periodic task drives its EWMAs.
    overload: OverloadSignal,
    /// The running replication supervisor (kept alive so its pullers + reconcile
    /// loop are not dropped). `None` on an unwired node.
    supervisor: Option<ReplicationSupervisor>,
    /// The replicating call store when replication is wired (`None` otherwise),
    /// re-exposed so the failover harness can introspect/assert replica
    /// presence (`get_call(role, primary, call_ref)`).
    repl_store: Option<Arc<ReplicatingCallStore>>,
    /// Abort handles for the directly-spawned tasks (router loop + repl serve
    /// loop). [`abort`](Self::abort) aborts them for a simulated crash; ordinary
    /// drop leaves them to die with the endpoint/channels.
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// What stopping this worker's authoring reaches: closed by a simulated
    /// crash and when the worker is superseded (ADR-0055 §6).
    fence: Arc<Fence>,
    /// The transaction owner and router loops: their pulses and tasks.
    loops: crate::loops::WorkerLoops,
    /// The X11 fail-back command sender of a wired node, retained so the
    /// channel the router selects on stays open while the core lives. `None`
    /// on an unwired node: the receiver is created in the same arm as this
    /// sender, so the router then has none to poll.
    _repl_tx: Option<tokio::sync::mpsc::UnboundedSender<router::ReplCommand>>,
    /// The node this core serves: the scope it was spawned under.
    node: NodeScope,
}

impl B2buaCore {
    /// Build over an already-bound endpoint and spawn the router loop with no
    /// callflow services registered (the composed rule list is exactly
    /// `default_rules()`).
    pub fn spawn(endpoint: Box<dyn UdpEndpoint>, deps: B2buaDeps) -> Self {
        Self::spawn_with_services(endpoint, deps, Vec::new())
    }

    /// Like [`spawn`](Self::spawn) but registers `services` (ADR-0016): each
    /// service's state-gated rules are composed above the core defaults and its
    /// `init` runs at call setup. Out-of-tree services (e.g. `announcement`) are
    /// injected here by the host process / harness, keeping `b2bua` free of any
    /// dependency on them.
    pub fn spawn_with_services(
        endpoint: Box<dyn UdpEndpoint>,
        deps: B2buaDeps,
        services: Vec<ServiceDef>,
    ) -> Self {
        Self::spawn_with_overload(endpoint, deps, services, None)
    }

    /// Like [`spawn_with_services`](Self::spawn_with_services) but lets the caller
    /// **inject the worker-side [`OverloadSignal`]** the periodic sampler task
    /// drives and every OPTIONS-200 `X-Overload` header reads. `None` builds a
    /// fresh [`OverloadSignal::live`], whose live busy-ratio sampler reads ~0
    /// ELU under a healthy/paused runtime.
    ///
    /// This is the sampler-injection seam (mirroring `start_with_config` /
    /// `spawn_with_services`): a `start_paused` test passes an `OverloadSignal`
    /// built over the `simulated()` sampler, sets a non-zero ELU through its
    /// control, advances a few [`SAMPLE_PERIOD`](OverloadSignal::SAMPLE_PERIOD)s,
    /// and observes the published header's `elu` rise above 0 — driving the
    /// injected value THROUGH the running sampler task into the EWMA and the
    /// header (the live sampler alone cannot exercise this: its busy ratio
    /// stays ~0 under a paused runtime).
    ///
    /// The core serves the [`NodeScope`] it is spawned under: every task it
    /// spawns keeps that node.
    pub fn spawn_with_overload(
        endpoint: Box<dyn UdpEndpoint>,
        deps: B2buaDeps,
        services: Vec<ServiceDef>,
        overload: Option<OverloadSignal>,
    ) -> Self {
        let B2buaDeps {
            config,
            decision,
            limiter,
            cdr,
            store,
            store_faults,
            wire_faults,
            turn_probe,
            clock,
            id_gen,
            refusals,
            deferred_ceilings,
            replication,
            metrics,
            adaptation_http,
            compose,
            capacity,
        } = deps;
        let node = NodeScope::current();

        let parser: Arc<dyn SipParser + Send + Sync> = Arc::new(CustomParser::new());
        let refusals = refusals.unwrap_or_else(|| {
            Refusals::new(
                config.retry_after_base_sec,
                config.retry_after_jitter_sec,
                sip_txn::REFUSED_MEMO_MAX,
                &id_gen,
            )
        });
        refusals.advertise(&config.minted_final_advertisement);
        let txn_config = txn_config(&config, &id_gen, &refusals, deferred_ceilings);
        let (txn, txn_rx) = TransactionLayer::spawn(endpoint, parser, txn_config);
        let (timers, timer_rx) = TimerService::spawn_with_metrics(clock.clone(), metrics.clone());

        let mut state = CallState::new(store, config.self_ordinal.clone(), metrics.clone())
            .with_clock(clock.clone());

        // Abort handles for the directly-spawned tasks (serve loop + router).
        // Collected so a harness can simulate a crash by aborting them.
        let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        // The tasks the worker ends with (`WorkerLoops::with_vital`).
        let mut vital = vec![("timer_driver", timers.task())];
        vital.extend(cdr.task().map(|task| ("cdr_drainer", task)));

        // Replication wiring (opt-in). When present: write the store directly,
        // serve our changelog, start the puller supervisor, gate readiness on
        // it, and open the X11 fail-back command channel (puller → router). An
        // unwired node constructs none of these: no store write path, no
        // channel, no receiver for the router to poll.
        let repl_store = replication.as_ref().map(|s| s.store.clone());
        let replica = replication.as_ref().map(|s| s.replica).unwrap_or_default();
        let hold_cap = replication.as_ref().map_or(Duration::ZERO, |s| s.write_path.hold_cap);
        let capacity = capacity.unwrap_or_else(CapacityGate::live);
        capacity.configure(&config.capacity);
        capacity.configure_backup(&replica.backup);
        let (readiness, supervisor, fail_back) = match replication {
            Some(setup) => {
                let self_ordinal = config.self_ordinal.clone();
                setup.store.changelog().set_backlog_marks(setup.write_path.backlog);
                let (repl_tx, repl_rx) =
                    tokio::sync::mpsc::unbounded_channel::<router::ReplCommand>();
                // Stamp the replicated-body TTL with the **reboot budget** (ADR-0011
                // X11): an orphaned backup Element self-evicts after it, not after the
                // 1 h max_duration backstop (`ReplicaConfig` outlasts one keepalive).
                let replicated_ttl_ms = replica.reboot_budget_sec.saturating_mul(1000);
                state = state
                    .with_replication(setup.store.clone())
                    .with_replicated_ttl_ms(replicated_ttl_ms);

                let self_identity =
                    (!config.self_identity.is_empty()).then(|| config.self_identity.clone());
                let supervisor = ReplicationSupervisor::new(
                    self_ordinal.clone(),
                    self_identity,
                    setup.network.clone(),
                    (*setup.store).clone(),
                    setup.addr_resolver.clone(),
                    setup.puller,
                    metrics.clone(),
                );
                // Pullers forward X11 fail-back commands to the router; wire the
                // sink BEFORE `start` so the initial pullers carry it.
                supervisor.set_repl_sink(RouterSink::new(repl_tx.clone(), state.takeover_probe()));
                supervisor.set_capacity(capacity.clone());
                supervisor.set_turn_probe(turn_probe.clone());
                supervisor.start(setup.membership.clone());

                // Serve our changelog to pulling peers. `ReplServer` reads bodies
                // from the same replicating store (as a `BodySource`). No handback
                // signal rides the wire under ADR-0014 — a backup self-releases its
                // takeover copies on transaction completion, and reconciliation is
                // the `(p,b)` version vector — so the server just streams changelog.
                let server = ReplServer::new(
                    self_ordinal,
                    setup.store.changelog().clone(),
                    setup.store.clone(),
                )
                .with_metrics(metrics.clone())
                .with_config(setup.serve);
                let server = spawn_keeping_node(server.run(setup.listener));
                vital.push(("repl_server", server.abort_handle()));
                tasks.push(server);

                // Context rebuild is **puller-driven and continuous** (ADR-0014):
                // each bootstrap pass signals `ReclaimAll` itself (puller.rs
                // `signal_reclaim_all`) the instant it has imported bodies, and the
                // steady-state tail materialises later reverse-flush stragglers per
                // call. There is no bounded go-active sweep (one would strand every
                // call landing after its cliff) and no go-active handshake — a
                // rebooting node rebuilds from what it has pulled and keeps pulling.

                let readiness = Readiness::new(Arc::new(supervisor.clone()))
                    .with_incarnation(setup.incarnation_gen);
                // The worker's own withdrawal from routing latches Draining
                // whether or not SIGTERM has arrived (ADR-0031 D6). A watch, not
                // a handle held by the supervisor: readiness already owns the
                // supervisor, and a cycle would outlive `abort`. A restarted run
                // reads the watch afresh.
                let watched = supervisor.clone();
                let latch = readiness.clone();
                tasks.push(crate::supervise::spawn(
                    crate::supervise::WITHDRAWAL_LATCH,
                    move || {
                        let mut self_endpoint = watched.self_endpoint();
                        let latch = latch.clone();
                        async move {
                            loop {
                                if self_endpoint.borrow_and_update().is_withdrawn() {
                                    latch.set_withdrawn();
                                    return;
                                }
                                if self_endpoint.changed().await.is_err() {
                                    return;
                                }
                            }
                        }
                    },
                ));
                (readiness, Some(supervisor), Some((repl_tx, repl_rx)))
            }
            // Unwired node: always-200 OPTIONS, no replication part at all; its
            // boot instant names the process.
            None => (
                Readiness::always_ready()
                    .with_incarnation(u64::try_from(clock.now_ms()).unwrap_or(0)),
                None,
                None,
            ),
        };
        // One `Option` holds both halves of the fail-back channel: the router's
        // receiver exists only alongside the retained sender.
        let (repl_tx, repl_rx) = fail_back.unzip();

        // The re-entry channel feeds both fire-and-forget results and the call
        // reaper's verdicts; created before the dispatcher so the reaper's
        // failure hook (two-strike panic escalation, ADR-0020 X6) can be wired
        // into the per-call workers at construction.
        let (reentry_tx, reentry_rx) = tokio::sync::mpsc::unbounded_channel();
        let reaper = crate::reaper::Reaper::new(
            config.reaper_enabled,
            config.reaper_sweep_interval_sec,
            config.reaper_idle_max_ms(),
            reentry_tx.clone(),
            metrics.clone(),
        );
        let dispatcher = PerCallDispatcher::new(
            config.event_dispatch_concurrency,
            config.per_call_queue_depth,
            config.per_call_queue_cap,
            metrics.clone(),
        )
        .with_new_call_bounds(config.new_call_permits(), config.new_call_queue_headroom())
        .with_failure_hook(reaper.failure_hook());
        // Only the reaper's verdict ends a capped call: without the reaper the
        // cap would refuse the call's traffic and nothing would end it.
        let dispatcher = if config.reaper_enabled {
            dispatcher.with_lifetime_cap(config.max_messages_per_call_lifetime)
        } else {
            dispatcher
        };
        // Compose the registered services' state-gated rules above the core
        // defaults (ADR-0016). With an empty `services` this is exactly
        // `default_rules()`. Note: in-tree `transfer` rides
        // `default_rules()` directly (its cursor is a projection), so it is not in
        // this list; `services` here is for `init`-seeded services (e.g. the
        // out-of-tree `announcement` capstone).
        let rules = compose_rules(&services, default_rules_with(&compose));
        // Worker-side overload signal. A live ELU/GC sampler backs
        // it by default; the periodic task below drives `sample()` at the 100 ms
        // cadence so the EWMAs published on every OPTIONS-200 `X-Overload` header
        // track load. A test may inject one over the `simulated()` sampler (the
        // sampler-injection seam) to drive a known ELU through the running task.
        let overload = overload.unwrap_or_else(OverloadSignal::live);
        // The panic-ELU and bucket rungs' inputs: seed the CPS token bucket and
        // the panic-ELU threshold from the now-final config (the harness
        // `tune` seam ran in `spawn_b2bua_core` before this). Must happen before
        // `config` is moved into the ctx below.
        overload.configure_admission(&config);
        // Decision-backend deadline (ADR-0022): bound EVERY engine round-trip so
        // a hung adapter cannot wedge a per-call worker past
        // `call_control_timeout_ms` — the caller heard the auto-100 and must get
        // its final. Wrapped HERE, after the `tune` seam finalized the config, so
        // no injection path bypasses it. `<= 0` disables (reaper-wedge escape).
        let decision =
            crate::decision::DeadlineDecisionEngine::wrap(decision, config.call_control_timeout_ms);
        // The worker's handle on its call limiter: the lease, the release
        // queue, the refresh batch and the bounded, breaker-guarded admit
        // path. Their tasks join the node's, aborted with them.
        let (limiter, limiter_tasks) = crate::limiter::LimiterWorker::start(
            limiter,
            &config,
            metrics.clone(),
            reentry_tx.clone(),
        );
        tasks.extend(limiter_tasks);
        let options_tagger = sip_txn::StatelessTagger::from_id_gen(&id_gen);
        let ctx = Arc::new(RouterCtx {
            outbound_proxy: crate::router::outbound_proxy_target(&config),
            config,
            replica,
            state,
            store_faults,
            wire_faults,
            turn_probe,
            txn,
            router_pulse: Arc::new(sip_txn::LoopPulse::new()),
            timers,
            dispatcher,
            reaper: reaper.clone(),
            decision,
            limiter,
            cdr: cdr.clone(),
            id_gen,
            clock,
            rules: Arc::new(rules),
            services: Arc::new(services),
            metrics: metrics.clone(),
            // The two core obligation kinds (limiter, CDR); a future service-
            // contributed kind registers here via `.with(...)` (ADR-0020 X7).
            obligations: Arc::new(crate::obligations::ObligationSet::core()),
            readiness: readiness.clone(),
            options_tagger,
            departed: supervisor.as_ref().map(crate::repl::departed_probe),
            overload: overload.clone(),
            capacity: capacity.clone(),
            refusals: refusals.clone(),
            unborn: Default::default(),
            keepalive_waves: crate::lifecycle::keepalive_timeout_waves(),
            unroutable_waves: crate::lifecycle::UnroutableWaves::new(),
            late_pracks: Default::default(),
            holds: router::Holds::new(hold_cap),
            reentry_tx,
            // Arc-shared into every per-call `ctx.clone()`, like `decision`.
            adaptation_http: adaptation_http.map(Arc::new),
        });

        let run = router::run(ctx.clone(), txn_rx, timer_rx, reentry_rx, repl_rx);
        let router = spawn_keeping_node(run);
        let owner = (ctx.txn.owner_pulse().clone(), ctx.txn.owner_task());
        let routing = (ctx.router_pulse.clone(), router.abort_handle());
        let loops = vital
            .into_iter()
            .fold(crate::loops::WorkerLoops::new(owner, routing), |loops, (name, task)| {
                loops.with_vital(name, task)
            });
        tasks.push(router);
        // The single periodic sweep task, driving two concerns off ONE
        // `tokio::time::interval` (`sweep::run`), each step behind its own panic
        // boundary, counted per step. Aborted by the harness `crash()` like the
        // router/serve loops. Per tick, in order:
        //   1. the reaper sweep (ADR-0020): scan the last-touched ledger + inject
        //      verdicts through the re-entry channel — `maybe_sweep` is a no-op for
        //      a disabled reaper.
        //   2. the Model-Y replica-store maintenance (ADR-0020 X3): the one
        //      eviction site of expired replica bodies (missed-delete ghosts AND a
        //      deferred terminal whose primary never reclaimed it), plus the
        //      resurrection-tombstone prune. **No discharge** — the primary is the
        //      sole discharge authority; an evicted deferred terminal has its
        //      limiter key released and its CDR counted lost. No-op without a
        //      replicating store.
        // The two gates are independent (reaper `enabled` vs replica store present),
        // so neither disabling the reaper nor running without HA suppresses the
        // other. The harness `advance` drives both under the paused clock.
        {
            let reaper = reaper.clone();
            let state = ctx.state.clone();
            let in_flight = ctx.dispatcher.in_flight();
            let ctx2 = ctx.clone();
            let interval = std::time::Duration::from_millis(reaper.sweep_interval_ms());
            let (m1, m2) = (ctx.metrics.clone(), ctx.metrics.clone());
            let reaper_ctx = ctx.clone();
            let reaper_step = crate::sweep::SweepStep::new(
                "reaper",
                move || {
                    let (reaper, state, in_flight, ctx) =
                        (reaper.clone(), state.clone(), in_flight.clone(), reaper_ctx.clone());
                    async move { reaper.maybe_sweep(&state, &in_flight, ctx.clock.now_ms()) }
                },
                move || m1.bump_reaper_sweep_panic(),
            );
            let replica_step = crate::sweep::SweepStep::new(
                "replica_reap",
                move || {
                    let ctx = ctx2.clone();
                    async move { router::reap_expired_replicas(&ctx, ctx.clock.now_ms()).await }
                },
                move || m2.bump_replica_reap_panic(),
            );
            let sweep = crate::sweep::run(interval, vec![reaper_step, replica_step]);
            tasks.push(spawn_keeping_node(sweep));
        }

        // The worker-side load sampler. Rides `tokio::time::interval` so a
        // paused-clock test advances it with `tokio::time::advance` like every
        // other behaviour
        // timer (CLAUDE.md: behaviour rides `tokio::time` directly). Each tick
        // reads the ELU/GC sampler and feeds the EWMAs published on `X-Overload`,
        // then samples the capacity gate (RSS + the level the ingress brake reads).
        // Aborted with the other tasks on a simulated `crash()`.
        tasks.push(samplers::spawn_overload_sampler(overload.clone(), ctx.clone()));

        // Clock-skew divergence sampler (clock-skew hardening observability),
        // aborted with the other tasks on a simulated `crash()`.
        tasks.push(samplers::spawn_clock_divergence(ctx.clock.clone(), metrics.clone()));

        // A worker superseded by a recreation of its ordinal stops authoring
        // the instant it observes it (ADR-0055 §6).
        let fence = Arc::new(Fence {
            tasks: tasks.iter().map(|t| t.abort_handle()).collect(),
            ctx: Arc::downgrade(&ctx),
            supervisor: supervisor.clone(),
        });
        tasks.push(spawn_keeping_node(fence.clone().on_superseded()));

        Self {
            ctx,
            metrics,
            cdr,
            refusals,
            readiness,
            overload,
            supervisor,
            repl_store,
            tasks,
            fence,
            loops,
            _repl_tx: repl_tx,
            node,
        }
    }

    /// The router context, for the in-crate unit tests that drive one router
    /// seam directly (`router::materialise`) over a fully wired core.
    #[cfg(test)]
    pub(crate) fn router_ctx(&self) -> &Arc<RouterCtx> {
        &self.ctx
    }

    /// The fail-back command sender the router's receiver pairs with, for the
    /// tests that assert what an unwired node constructs.
    #[cfg(test)]
    pub(crate) fn fail_back_sender(
        &self,
    ) -> Option<&tokio::sync::mpsc::UnboundedSender<router::ReplCommand>> {
        self._repl_tx.as_ref()
    }

    /// The replicating call store, when replication is wired (`None` on an
    /// unwired node). The failover harness reads it to assert a replica
    /// landed on the backup (`get_call`) and to introspect the reclaimed gen.
    pub fn repl_store(&self) -> Option<&Arc<ReplicatingCallStore>> {
        self.repl_store.as_ref()
    }

    /// The replication supervisor, when wired (`None` on an unwired node). The
    /// failover harness reads its `is_ready`/`all_bootstrapped`/`all_current`
    /// gates to mark a rebooted worker alive in the proxy registry.
    pub fn supervisor(&self) -> Option<&ReplicationSupervisor> {
        self.supervisor.as_ref()
    }

    /// The worker-side overload signal. Callers advance the `adm`
    /// counter on a non-emergency new-dialog admit
    /// ([`OverloadSignal::increment_non_emergency_admitted`]) and read the
    /// published `X-Overload` header; a periodic task drives its EWMAs.
    pub fn overload(&self) -> &OverloadSignal {
        &self.overload
    }

    /// The worker's refusals of new INVITEs, shared with its transaction
    /// layer: the instance an ingress brake installed after the core takes.
    pub fn refusals(&self) -> &Refusals {
        &self.refusals
    }

    /// The memory admission gate (ADR-0037) the running core decides with.
    pub fn capacity(&self) -> &CapacityGate {
        &self.ctx.capacity
    }

    /// This worker's handle on its call limiter: an exit that waits for no
    /// call sends its queued releases last through it
    /// ([`flush`](crate::limiter::LimiterWorker::flush)), or gives them up
    /// ([`give_up_all`](crate::limiter::LimiterWorker::give_up_all)).
    pub fn limiter(&self) -> &crate::limiter::LimiterWorker {
        &self.ctx.limiter
    }

    /// The transaction owner and router loops: whether each turns and still
    /// runs.
    pub fn loops(&self) -> &crate::loops::WorkerLoops {
        &self.loops
    }

    pub fn metrics(&self) -> &B2buaMetrics {
        &self.metrics
    }

    /// The transaction layer's metrics handle (events-channel depth/capacity,
    /// per-reason drop counters, active transactions). Exposed so the host can
    /// render the txn-level backpressure signals the `B2buaMetrics` set omits —
    /// notably `event_queue_drops{reason="response"}`, the keepalive-response
    /// shedding that silently tears down established dialogs under a new-call burst.
    pub fn txn_metrics(&self) -> &sip_txn::TransactionMetrics {
        self.ctx.txn.metrics()
    }

    pub fn cdr(&self) -> &Arc<dyn CdrWriter> {
        &self.cdr
    }

    /// Sample the store + replication map sizes into the memory-attribution
    /// gauges (`b2bua_store_*`, `b2bua_repl_meta_*`, `b2bua_repl_changelog_*`).
    /// Called on a slow cadence by the runner so a RSS climb can be pinned to a
    /// specific map even when `active_calls` is flat. Cheap: a couple of brief
    /// locks, off the hot path.
    pub fn sample_gauges(&self) {
        self.ctx.state.sample_store_gauges();
        if let Some(repl) = &self.repl_store {
            let (meta_total, meta_backup) = repl.meta_counts();
            let (entries, peers) = repl.changelog().depth();
            let unbacked = repl.unbacked_count() as u64;
            self.metrics.set_repl_store_gauges(meta_total, meta_backup, unbacked, entries, peers);
        }
    }
}

#[cfg(test)]
mod txn_config_tests;
