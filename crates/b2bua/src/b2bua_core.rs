//! `B2buaCore` — composes the dispatcher + router + call store + transaction
//! layer + timer service + decision engine + CDR writer over a bound UDP
//! endpoint, and spawns the router loop. Construct it over an endpoint (in
//! tests, `Harness::bind_sut`), then drive SIP at the endpoint's address.

use std::sync::Arc;

use observe::{spawn_keeping_node, NodeScope};
use sip_message::parser::custom::CustomParser;
use sip_message::SipParser;
use sip_net::UdpEndpoint;
use sip_txn::{DeferredBound, IdGen, TransactionConfig, TransactionLayer};

use crate::admission::Refusals;

use crate::capacity::{CapacityGate, Occupancy};
use crate::cdr::CdrWriter;
use crate::config::B2buaConfig;
use crate::dispatch::PerCallDispatcher;
use crate::metrics::B2buaMetrics;
use crate::overload::OverloadSignal;
use crate::repl::{Readiness, ReplServer, ReplicatingCallStore, ReplicationSupervisor, RouterSink};
use crate::router::{self, RouterCtx};
use crate::rules::{compose_rules, default_rules_with, ServiceDef};
use crate::store::{BufferedTerminateWriter, CallState, CallStore};
use crate::timers::TimerService;

mod deps;
pub use deps::{B2buaDeps, ReplicationSetup};

mod drain;
#[cfg(test)]
mod drain_tests;
#[cfg(test)]
mod loops_tests;
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
                &IdGen::from_entropy(),
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

        // Replication wiring (opt-in). When present: drain the store write path
        // through a buffered writer into the replicating store, serve our
        // changelog, start the puller supervisor, gate readiness on it, and open
        // the X11 fail-back command channel (puller → router). An unwired node
        // constructs none of these: no writer task, no channel, no receiver for
        // the router to poll.
        let repl_store = replication.as_ref().map(|s| s.store.clone());
        let capacity = capacity.unwrap_or_else(CapacityGate::live);
        capacity.configure(&config.capacity);
        let (readiness, supervisor, fail_back) = match replication {
            Some(setup) => {
                let self_ordinal = config.self_ordinal.clone();
                // The writer drains to the replicating store itself so its
                // changelog bumps on every flush carrying a peer.
                let writer =
                    BufferedTerminateWriter::spawn(setup.store.clone() as Arc<dyn CallStore>, 1024);
                let (repl_tx, repl_rx) =
                    tokio::sync::mpsc::unbounded_channel::<router::ReplCommand>();
                // Route flushes/removes for backed-up calls through the policy, and
                // stamp the replicated-body TTL with the operator's **reboot budget**
                // (ADR-0011 X11): an orphaned backup Element self-evicts after the
                // budget rather than the 1 h max_duration backstop. The budget is a
                // config knob in its own right (decoupled from the keepalive, though
                // `config.validate()` guarantees it outlasts one keepalive refresh
                // gap so a healthy idle call's backup is never evicted prematurely).
                let replicated_ttl_ms = config.reboot_budget_sec.saturating_mul(1000);
                state = state
                    .with_replication(setup.store.clone(), writer)
                    .with_replicated_ttl_ms(replicated_ttl_ms);

                let supervisor = ReplicationSupervisor::new(
                    self_ordinal.clone(),
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
                tasks.push(spawn_keeping_node(server.run(setup.listener)));

                // Context rebuild is **puller-driven and continuous** (ADR-0014):
                // each bootstrap pass signals `ReclaimAll` itself (puller.rs
                // `signal_reclaim_all`) the instant it has imported bodies, and the
                // steady-state tail materialises later reverse-flush stragglers per
                // call. There is no bounded go-active sweep (one would strand every
                // call landing after its cliff) and no go-active handshake — a
                // rebooting node rebuilds from what it has pulled and keeps pulling.

                let readiness = Readiness::new(Arc::new(supervisor.clone()));
                // The worker's own withdrawal from routing latches Draining
                // whether or not SIGTERM has arrived (ADR-0031 D6). A watch, not
                // a handle held by the supervisor: readiness already owns the
                // supervisor, and a cycle would outlive `abort`.
                let mut self_endpoint = supervisor.self_endpoint();
                let latch = readiness.clone();
                tasks.push(spawn_keeping_node(async move {
                    loop {
                        if self_endpoint.borrow_and_update().is_withdrawn() {
                            latch.set_withdrawn();
                            return;
                        }
                        if self_endpoint.changed().await.is_err() {
                            return;
                        }
                    }
                }));
                (readiness, Some(supervisor), Some((repl_tx, repl_rx)))
            }
            // Unwired node: always-200 OPTIONS, no replication part at all.
            None => (Readiness::always_ready(), None, None),
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
        let ctx = Arc::new(RouterCtx {
            outbound_proxy: crate::router::outbound_proxy_target(&config),
            config,
            state,
            store_faults,
            wire_faults,
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
            overload: overload.clone(),
            capacity: capacity.clone(),
            refusals: refusals.clone(),
            unborn: Default::default(),
            keepalive_waves: crate::lifecycle::keepalive_timeout_waves(),
            unroutable_waves: crate::lifecycle::UnroutableWaves::new(),
            late_pracks: Default::default(),
            reentry_tx,
            // Arc-share the injected port into every per-call `ctx.clone()`,
            // exactly like `decision`/`limiter`.
            adaptation_http: adaptation_http.map(Arc::new),
        });

        let run = router::run(ctx.clone(), txn_rx, timer_rx, reentry_rx, repl_rx);
        let router = spawn_keeping_node(run);
        let owner = (ctx.txn.owner_pulse().clone(), ctx.txn.owner_task());
        let routing = (ctx.router_pulse.clone(), router.abort_handle());
        let loops = crate::loops::WorkerLoops::new(owner, routing);
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
        // Aborted with the other tasks on a simulated `crash()`. This task owns no
        // per-call state, so it needs no release path.
        {
            let overload = overload.clone();
            let ctx2 = ctx.clone();
            tasks.push(spawn_keeping_node(async move {
                let mut tick = tokio::time::interval(OverloadSignal::SAMPLE_PERIOD);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                tick.tick().await; // skip the immediate first tick (TS first fire is +100 ms)
                loop {
                    tick.tick().await;
                    overload.sample();
                    ctx2.capacity.sample(Occupancy {
                        calls: ctx2.unborn.with_live(|| ctx2.state.active_count() as u64),
                        transactions: ctx2.txn.metrics().active_transactions() as u64,
                    });
                }
            }));
        }

        // Clock-skew divergence sampler (clock-skew hardening observability).
        // Every ~30 s, read the RAW system wall clock and compare it to this
        // node's monotonic-anchored `Clock::now_ms` — `now_ms` does NOT follow a
        // host NTP step, so the gap names exactly the event that skews cross-node
        // replicated timer deadlines. Publishes `clock_wall_divergence_ms` and
        // rate-limits a warn line when the magnitude crosses 500 ms.
        // Observability only: does NOT re-anchor the clock (timestamps stay
        // monotonic; the behavioural fix is the replication-boundary re-anchor).
        // Rides `tokio::time::interval` so a paused-clock test advances it too;
        // aborted with the other tasks on a simulated `crash()`.
        {
            let clock = ctx.clock.clone();
            let metrics = metrics.clone();
            tasks.push(spawn_keeping_node(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                tick.tick().await; // skip the immediate first tick
                let mut warned_recently = false;
                loop {
                    tick.tick().await;
                    let divergence = clock.wall_divergence_ms(sip_clock::raw_system_wall_ms());
                    metrics.set_clock_wall_divergence_ms(divergence);
                    if divergence.abs() > 500 {
                        // Rate-limit: warn on the RISING edge only, so a sustained
                        // step logs once, not every 30 s.
                        if !warned_recently {
                            tracing::warn!(
                                divergence_ms = divergence,
                                "clock skew: wall-clock divergence (raw SystemTime - monotonic \
                                 Clock::now_ms) — likely a host NTP step; cross-node replicated \
                                 timer deadlines may skew. Fix is INFRA (slewing chrony + host \
                                 kept awake), NOT the SUT."
                            );
                            warned_recently = true;
                        }
                    } else {
                        warned_recently = false;
                    }
                }
            }));
        }

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

    /// The buffered terminate writer the call store write path submits to, for
    /// the tests that assert what an unwired node constructs.
    #[cfg(test)]
    pub(crate) fn terminate_writer(&self) -> Option<&BufferedTerminateWriter> {
        self.ctx.state.terminate_writer()
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

    /// This core's readiness handle (clone-cheap, shares the latches). Exposed
    /// so a harness can observe the node's own drain/readiness state after the
    /// core itself is no longer reachable.
    pub fn readiness(&self) -> crate::repl::Readiness {
        self.readiness.clone()
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

    /// Readiness gate, routed through the SAME latched [`Readiness`] state
    /// machine the SIP OPTIONS self-report uses (X6 anti-flap). With ready-gated
    /// EndpointSlice membership, an un-latched predicate would flip back to 503
    /// on a desired-but-not-yet-current peer and unpublish the node, and a
    /// simultaneously-restarted cluster would oscillate published↔unpublished.
    /// The latch makes Ready sticky on both probe surfaces; `Draining` reports
    /// not-ready (the probe's job during drain is to unpublish).
    pub fn is_ready(&self) -> bool {
        self.readiness.state() == crate::repl::ReadinessState::Ready
    }

    /// The full 3-valued readiness state (`NotReady`/`Ready`/`Draining`). The
    /// runner maps it onto the HTTP probe's `ProbeState` so `/ready` can report
    /// `draining` distinctly from `not-ready`, matching the OPTIONS self-report.
    pub fn readiness_state(&self) -> crate::repl::ReadinessState {
        self.readiness.state()
    }

    /// CRASH: abort the directly-spawned tasks (serve loop + router) and park
    /// every replication puller (closing its pulled connections). Mirrors the
    /// ha-harness `HaNode::crash` discipline at the live-core level: the spawned
    /// per-connection/per-puller tasks lose their driver and unwind, and dropping
    /// this `B2buaCore` afterwards releases the last store/supervisor `Arc`s so a
    /// reboot can re-listen on the same addresses. No per-call body runs on.
    pub fn abort(&mut self) {
        for t in self.tasks.drain(..) {
            t.abort();
        }
        self.ctx.dispatcher.halt();
        // Also abort the transaction-layer owner task — it is spawned untracked
        // inside `TransactionLayer::spawn` and owns the SIP endpoint, so without
        // this the "crashed" node keeps answering SIP (100/200/487, cached replays,
        // retransmits) until every surviving per-call task drops its cmd_tx clone.
        self.ctx.txn.abort_owner();
        // A crash loses the release queue; a drain still waiting flushes nothing.
        self.ctx.limiter.stop();
        if let Some(s) = &self.supervisor {
            s.shutdown();
        }
        // Per-call root spans are this core's runtime state, not replicated
        // state: they die with the core and return their active-trace slots
        // (ADR-0026). A survivor taking one of these calls over therefore opens
        // its OWN linked root, exactly as it does across a process boundary.
        let traces = crate::trace::traces();
        self.node.enter(|| {
            for call_ref in self.ctx.state.live_call_refs() {
                traces.close(&call_ref);
            }
        });
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

    /// Active call count (test/observability).
    pub fn active_calls(&self) -> usize {
        self.ctx.state.active_count()
    }

    /// [`active_calls`](Self::active_calls) as an OWNED probe — the live-call
    /// input of [`drain_probe`](Self::drain_probe), handed out on its own for a
    /// caller that watches quiescence alone. Read-only.
    pub fn active_calls_probe(&self) -> Arc<dyn Fn() -> usize + Send + Sync> {
        let ctx = self.ctx.clone();
        Arc::new(move || ctx.state.active_count())
    }

    /// Does this worker currently **serve** `call_ref` (hold it live in its call
    /// map — i.e. it would emit the call's keepalive and answer in-dialog traffic)?
    /// The cluster-level invariant the failover tests assert is "exactly one node
    /// serves a given call". Test/observability.
    pub fn serves(&self, call_ref: &str) -> bool {
        self.ctx.state.peek(call_ref).is_some()
    }

    /// Cancel a timer in this node's driver, the record untouched: a test's
    /// stand-in for a fire the per-call queue dropped.
    pub async fn cancel_driver_timer(&self, call_ref: &str, key: &call::TimerKey) {
        self.ctx.timers.cancel(call_ref.to_string(), key.clone()).await;
    }

    /// Post `event` to the router as a timer driver or a callout posts one: a
    /// test's stand-in for an event already in flight when it is posted.
    pub fn post_event(&self, event: b2bua_sdk::event::CallEvent) {
        let _ = self.ctx.reentry_tx.send(event);
    }

    /// The live copy of `call_ref` this worker serves, if any (introspection:
    /// what its rules read at the next event).
    pub fn live_call(&self, call_ref: &str) -> Option<call::Call> {
        self.ctx.state.peek(call_ref)
    }

    /// Every call this worker serves, read through a probe that outlives a
    /// borrow of the core and reads `None` once the worker's state is gone
    /// (introspection: a failure dump's view of the live calls).
    pub fn live_calls_probe(&self) -> Arc<dyn Fn() -> Option<Vec<call::Call>> + Send + Sync> {
        let ctx = Arc::downgrade(&self.ctx);
        Arc::new(move || {
            let ctx = ctx.upgrade()?;
            let mut refs = ctx.state.live_call_refs();
            refs.sort();
            Some(refs.iter().filter_map(|r| ctx.state.peek(r)).collect())
        })
    }

    /// New calls admitted at ingress whose turn has not created their call
    /// yet; 0 once every admitted turn has run or been dropped.
    pub fn unborn_calls(&self) -> u64 {
        self.ctx.unborn.count()
    }

    /// Live per-call serialization-lock count (test/observability). Should track
    /// [`active_calls`](Self::active_calls); a gap is the orphan-reject lock leak.
    pub fn lock_count(&self) -> usize {
        self.ctx.state.lock_count()
    }

    /// Live last-touched ledger entries (the reaper's liveness stamps,
    /// ADR-0020 X4). Mirrors the call map by construction; a residue after
    /// teardown is a stamp leak (the harness reap oracle's 4th invariant).
    pub fn touched_count(&self) -> usize {
        self.ctx.state.touched_count()
    }

    /// Live setup-CANCEL marks (the decision-application drop guard). Cleared on
    /// every teardown path; a residue after teardown is a mark leak (the harness
    /// reap oracle's 5th invariant).
    pub fn setup_cancelled_count(&self) -> usize {
        self.ctx.state.setup_cancelled_count()
    }

    /// HARNESS SURGERY: drop the live in-memory copy of `call_ref` — map, index,
    /// lock, takeover mark — with NO store mutation (the `pri:`/`bak:` replica
    /// bodies stay). Recreates, deterministically, the rebooted-primary
    /// mid-reclaim state ("body imported into `pri:{self}`, not yet
    /// materialised") that the bulk-`ReclaimAll` race only produces under
    /// timing: the failover tests use it to pin the on-demand reclaim read-path.
    /// Test/observability only — production teardown goes through the router's
    /// `release_call`.
    pub fn drop_live_copy(&self, call_ref: &str) -> bool {
        self.ctx.state.drop_local(call_ref)
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
            let (cl_entries, cl_peers) = repl.changelog().depth();
            self.metrics.set_repl_store_gauges(meta_total, meta_backup, cl_entries, cl_peers);
        }
    }
}

/// The transaction layer's tunables for this worker: its event queue, the
/// deployment's INVITE bounds and CANCEL policy, the worker's shared
/// `refusals`, and the ceilings on the deferred backlog
/// ([`crate::admission::deferred_bound`]) unless `ceilings` sets its own.
fn txn_config(
    config: &B2buaConfig,
    id_gen: &Arc<IdGen>,
    refusals: &Refusals,
    ceilings: Option<DeferredBound>,
) -> TransactionConfig {
    let mut txn_config = TransactionConfig {
        // Sizes the events channel at 4096: room for a new-INVITE burst
        // beside the in-dialog traffic of established calls, whose keepalive
        // responses a full channel would drop.
        udp_queue_max: 1024,
        id_gen: id_gen.clone(),
        // The deployment's initial-INVITE bound (default 158 s):
        // `config.validate()` keeps `setup_timeout_ms` strictly below
        // it, so the rules path always gives up before the txn layer.
        invite_initial_timeout_ms: config.invite_txn_timeout_ms(),
        // The TU's first-response deadline on an initial INVITE
        // (ADR-0044; default Timer B, which arms nothing).
        invite_first_response_timeout_ms: config.invite_first_response_timeout_ms(),
        strict_to_tag: true,
        deferred_bound: None,
        invite_refusals: Some(refusals.memo().clone()),
    };
    txn_config.deferred_bound = Some(
        ceilings.unwrap_or_else(|| crate::admission::deferred_bound(txn_config.event_capacity())),
    );
    txn_config
}

#[cfg(test)]
mod txn_config_tests;
