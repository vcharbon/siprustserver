//! `B2buaCore` — composes the dispatcher + router + call store + transaction
//! layer + timer service + decision engine + CDR writer over a bound UDP
//! endpoint, and spawns the router loop. Port of `B2buaCore.ts`'s layer
//! composition. Construct it over an endpoint (in tests, `Harness::bind_sut`),
//! then drive SIP at the endpoint's address.

use std::net::SocketAddr;
use std::sync::Arc;

use repl_net::transport::ReplicationNetwork;
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::SipParser;
use sip_net::UdpEndpoint;
use sip_txn::{IdGen, TransactionConfig, TransactionLayer};
use topology::Membership;

use crate::cdr::CdrWriter;
use crate::config::B2buaConfig;
use crate::decision::CallDecisionEngine;
use crate::dispatch::PerCallDispatcher;
use crate::limiter::CallLimiter;
use crate::metrics::B2buaMetrics;
use crate::repl::{ReplServer, ReplicatingCallStore, ReplicationSupervisor, Readiness};
use crate::router::{self, RouterCtx};
use crate::rules::default_rules;
use crate::store::{BufferedTerminateWriter, CallState, CallStore};
use crate::timers::TimerService;

/// A running B2BUA worker. Holds the shared context; the router loop runs on a
/// spawned task that lives until the endpoint closes.
pub struct B2buaCore {
    ctx: Arc<RouterCtx>,
    metrics: B2buaMetrics,
    cdr: Arc<dyn CdrWriter>,
    /// Readiness handle (the supervisor-backed one when replication is wired,
    /// else the always-ready legacy one). Kept so [`begin_draining`] can latch it.
    readiness: Readiness,
    /// The running replication supervisor (kept alive so its pullers + reconcile
    /// loop are not dropped). `None` on the legacy/non-replicating path.
    supervisor: Option<ReplicationSupervisor>,
    /// The replicating call store when replication is wired (`None` otherwise),
    /// re-exposed so the S10b failover harness can introspect/assert replica
    /// presence (`get_call(role, primary, call_ref)`).
    repl_store: Option<Arc<ReplicatingCallStore>>,
    /// Abort handles for the directly-spawned tasks (router loop + repl serve
    /// loop). [`abort`](Self::abort) aborts them for a simulated crash; ordinary
    /// drop leaves them to die with the endpoint/channels as before.
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Retained X11 fail-back command sender — keeps the `repl_rx` channel the
    /// router selects on open even on the legacy path (no supervisor/puller holds
    /// a clone there), so a closed channel never busy-loops the router.
    _repl_tx: tokio::sync::mpsc::UnboundedSender<router::ReplCommand>,
}

/// Optional replication wiring for [`B2buaDeps`]. Supplying `Some(..)` turns a
/// `B2buaCore` into a replicating worker; `None` keeps today's behaviour exactly
/// (in-memory store, `always_ready()` OPTIONS, `PutOpts::default()` flush).
///
/// ## Seams deferred past S10a
/// - **`incarnation_gen`** — the per-boot incarnation seed for the
///   [`ReplicatingCallStore`]'s changelog (mirrors `IdGen::seeded`). S10a takes it
///   as an explicit input; **S11** derives the real source (e.g. a persisted /
///   monotonic boot counter) and feeds it here.
/// - **`addr_resolver`** — maps a cluster `Peer` to its replication
///   [`SocketAddr`], **resolved per connect** (ADR-0012 D3). S10b's sim harness
///   passes an explicit `ordinal → addr` map (`FnPeerResolver`);
///   **S11 (prod)** derives it from `ordinal + host + config`. We deliberately do
///   NOT invent an addressing grammar here — the resolver IS the seam.
pub struct ReplicationSetup {
    /// The replication transport (sim or real). The server `listen`s on it and
    /// the supervisor's pullers `connect` through it.
    pub network: Arc<dyn ReplicationNetwork>,
    /// Cluster membership (who to replicate to/from).
    pub membership: Arc<dyn Membership>,
    /// The replicating call store (built with `incarnation_gen`). Used as the
    /// `CallState` store AND served to pulling peers.
    pub store: Arc<ReplicatingCallStore>,
    /// Local replication listen address (where this node serves its changelog).
    pub listen_addr: SocketAddr,
    /// Resolves a peer to its replication address (the deferred S11 grammar seam).
    pub addr_resolver: crate::repl::AddrResolver,
    /// Per-boot incarnation seed for the changelog (deferred S11 real source).
    pub incarnation_gen: u64,
}

/// Wiring inputs for [`B2buaCore::spawn`].
pub struct B2buaDeps {
    pub config: B2buaConfig,
    pub decision: Arc<dyn CallDecisionEngine>,
    pub limiter: Arc<dyn CallLimiter>,
    pub cdr: Arc<dyn CdrWriter>,
    pub store: Arc<dyn CallStore>,
    pub clock: Clock,
    pub id_gen: Arc<IdGen>,
    /// Opt-in replication. `None` → today's non-replicating behaviour verbatim.
    pub replication: Option<ReplicationSetup>,
}

impl B2buaCore {
    /// Build over an already-bound endpoint and spawn the router loop.
    pub fn spawn(endpoint: Box<dyn UdpEndpoint>, deps: B2buaDeps) -> Self {
        let B2buaDeps {
            config,
            decision,
            limiter,
            cdr,
            store,
            clock,
            id_gen,
            replication,
        } = deps;
        let metrics = B2buaMetrics::new();

        let parser: Arc<dyn SipParser + Send + Sync> = Arc::new(CustomParser::new());
        let (txn, txn_rx) = TransactionLayer::spawn(
            endpoint,
            parser,
            TransactionConfig {
                udp_queue_max: 256,
                id_gen: id_gen.clone(),
            },
        );
        let (timers, timer_rx) = TimerService::spawn_with_metrics(clock.clone(), metrics.clone());

        // The store the terminate-writer drains to: the replicating store when
        // wired (so its changelog bumps on flushes carrying a peer), else the
        // caller's `dyn CallStore` (the in-memory legacy path).
        let drain_store: Arc<dyn CallStore> = match &replication {
            Some(s) => s.store.clone(),
            None => store.clone(),
        };
        let terminate_writer = BufferedTerminateWriter::spawn(drain_store, 1024);

        let mut state =
            CallState::new(store, terminate_writer, config.self_ordinal.clone(), metrics.clone());

        // Abort handles for the directly-spawned tasks (serve loop + router).
        // Collected so a harness can simulate a crash by aborting them.
        let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        // X11 fail-back command channel (puller/go-active → router). Created
        // unconditionally; `repl_tx` is retained on `Self` so the channel never
        // closes on the legacy (no-replication) path — otherwise `repl_rx.recv()`
        // would resolve `None` every poll and busy-loop the router select.
        let (repl_tx, repl_rx) =
            tokio::sync::mpsc::unbounded_channel::<router::ReplCommand>();

        // Replication wiring (opt-in). When present: serve our changelog, start
        // the puller supervisor, gate readiness on it, and route flushes through
        // the replicating store.
        let repl_store = replication.as_ref().map(|s| s.store.clone());
        let (readiness, supervisor) = match &replication {
            Some(setup) => {
                let self_ordinal = config.self_ordinal.clone();
                // Route flushes/removes for backed-up calls through the policy, and
                // stamp the replicated-body TTL with the operator's **reboot budget**
                // (ADR-0011 X11): an orphaned backup Element self-evicts after the
                // budget rather than the 1 h max_duration backstop. The budget is a
                // config knob in its own right (decoupled from the keepalive, though
                // `config.validate()` guarantees it outlasts one keepalive refresh
                // gap so a healthy idle call's backup is never evicted prematurely).
                let replicated_ttl_ms = config.reboot_budget_sec.saturating_mul(1000);
                state = state
                    .with_replication(setup.store.clone())
                    .with_replicated_ttl_ms(replicated_ttl_ms);

                // Start the topology-driven puller supervisor over the membership.
                let supervisor = ReplicationSupervisor::new(
                    self_ordinal.clone(),
                    setup.network.clone(),
                    (*setup.store).clone(),
                    setup.addr_resolver.clone(),
                    clock.clone(),
                    metrics.clone(),
                );
                // Pullers forward X11 fail-back commands to the router; wire the
                // sink BEFORE `start` so the initial pullers carry it.
                supervisor.set_repl_sink(repl_tx.clone());
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
                .with_metrics(metrics.clone());
                let network = setup.network.clone();
                let listen_addr = setup.listen_addr;
                tasks.push(tokio::spawn(async move {
                    match network.listen(listen_addr).await {
                        Ok(listener) => server.run(listener).await,
                        Err(_) => { /* bind failed — peers simply can't pull us */ }
                    }
                }));

                // Context rebuild is **puller-driven and continuous** (ADR-0014):
                // each bootstrap pass signals `ReclaimAll` itself (puller.rs
                // `signal_reclaim_all`) the instant it has imported bodies, and the
                // steady-state tail materialises later reverse-flush stragglers per
                // call. The old one-shot, readiness-gated, 10 s-bounded go-active
                // sweep is GONE: it stranded every call that landed after its cliff
                // (the endurance long-call-on-reboot 481). The reactive-only model
                // needs no go-active handshake — a rebooting node just rebuilds from
                // what it has pulled and keeps pulling.

                let readiness = Readiness::new(Arc::new(supervisor.clone()));
                (readiness, Some(supervisor))
            }
            // Legacy/default path: always-200 OPTIONS, no replication.
            None => (Readiness::always_ready(), None),
        };

        let dispatcher = PerCallDispatcher::new(
            config.event_dispatch_concurrency,
            config.per_call_queue_depth,
            config.per_call_queue_cap,
            metrics.clone(),
        );

        let (reentry_tx, reentry_rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = Arc::new(RouterCtx {
            config,
            state,
            txn,
            timers,
            dispatcher,
            decision,
            limiter,
            cdr: cdr.clone(),
            id_gen,
            clock,
            rules: Arc::new(default_rules()),
            metrics: metrics.clone(),
            readiness: readiness.clone(),
            reentry_tx,
        });

        tasks.push(tokio::spawn(router::run(
            ctx.clone(),
            txn_rx,
            timer_rx,
            reentry_rx,
            repl_rx,
        )));

        Self {
            ctx,
            metrics,
            cdr,
            readiness,
            supervisor,
            repl_store,
            tasks,
            _repl_tx: repl_tx,
        }
    }

    /// The replicating call store, when replication is wired (`None` on the
    /// legacy path). The S10b failover harness reads it to assert a replica
    /// landed on the backup (`get_call`) and to introspect the reclaimed gen.
    pub fn repl_store(&self) -> Option<&Arc<ReplicatingCallStore>> {
        self.repl_store.as_ref()
    }

    /// The replication supervisor, when wired (`None` on the legacy path). The
    /// failover harness reads its `is_ready`/`all_bootstrapped`/`all_current`
    /// gates to mark a rebooted worker alive in the proxy registry.
    pub fn supervisor(&self) -> Option<&ReplicationSupervisor> {
        self.supervisor.as_ref()
    }

    /// Readiness gate: every reachable peer bootstrapped AND current (the S7
    /// readiness state — read straight off the supervisor). Legacy (no
    /// replication) is always ready.
    pub fn is_ready(&self) -> bool {
        match &self.supervisor {
            Some(s) => s.all_bootstrapped() && s.all_current(),
            None => true,
        }
    }

    /// CRASH: abort the directly-spawned tasks (serve loop + router) and park
    /// every replication puller (closing its pulled connections). Mirrors the
    /// ha-harness `HaNode::crash` discipline at the live-core level: the spawned
    /// per-connection/per-puller tasks lose their driver and unwind, and dropping
    /// this `B2buaCore` afterwards releases the last store/supervisor `Arc`s so a
    /// reboot can re-listen on the same addresses. Intended for the failover
    /// harness only.
    pub fn abort(&mut self) {
        for t in self.tasks.drain(..) {
            t.abort();
        }
        if let Some(s) = &self.supervisor {
            s.shutdown();
        }
    }

    /// Latch this worker into the `Draining` readiness state (SIGTERM → drain).
    /// OPTIONS then self-reports `503 draining` so the front proxy steers new
    /// calls away while in-flight calls finish. Terminal — never un-drains.
    ///
    /// SIGTERM wiring: the **runner** (S11) should install a `tokio::signal`
    /// SIGTERM hook that calls this. We expose the method rather than installing
    /// the hook inside the library so tests/embedders control the signal surface.
    pub fn begin_draining(&self) {
        self.readiness.set_draining();
    }

    pub fn metrics(&self) -> &B2buaMetrics {
        &self.metrics
    }

    pub fn cdr(&self) -> &Arc<dyn CdrWriter> {
        &self.cdr
    }

    /// Active call count (test/observability).
    pub fn active_calls(&self) -> usize {
        self.ctx.state.active_count()
    }

    /// Does this worker currently **serve** `call_ref` (hold it live in its call
    /// map — i.e. it would emit the call's keepalive and answer in-dialog traffic)?
    /// The cluster-level invariant the failover tests assert is "exactly one node
    /// serves a given call". Test/observability.
    pub fn serves(&self, call_ref: &str) -> bool {
        self.ctx.state.peek(call_ref).is_some()
    }

    /// Live per-call serialization-lock count (test/observability). Should track
    /// [`active_calls`](Self::active_calls); a gap is the orphan-reject lock leak.
    pub fn lock_count(&self) -> usize {
        self.ctx.state.lock_count()
    }

    /// Sample the store + replication map sizes into the memory-attribution
    /// gauges (`b2bua_store_*`, `b2bua_repl_meta_*`, `b2bua_repl_changelog_*`).
    /// Called on a slow cadence by the runner so a RSS climb can be pinned to a
    /// specific map even when `active_calls` is flat — the lens that would have
    /// named the leak directly instead of by inference. Cheap: a couple of brief
    /// locks, off the hot path.
    pub fn sample_gauges(&self) {
        self.ctx.state.sample_store_gauges();
        if let Some(repl) = &self.repl_store {
            let (meta_total, meta_backup) = repl.meta_counts();
            let (cl_entries, cl_peers) = repl.changelog().depth();
            self.metrics
                .set_repl_store_gauges(meta_total, meta_backup, cl_entries, cl_peers);
        }
    }
}
