//! Test-only harness: binds a real [`b2bua::B2buaCore`] as a System-Under-Test
//! on the `scenario-harness` simulated network, so deterministic
//! alice ↔ b2bua ↔ bob flows run end-to-end through the recording. Extends the
//! `bind_sut` seam (ADR-0006/0009) to the B2BUA (ADR-0010).

use std::net::SocketAddr;
use std::sync::Arc;

use b2bua::cdr::{CdrRecord, InMemoryCdrWriter};
use b2bua::config::B2buaConfig;
use b2bua::decision::{CallDecisionEngine, ScriptedDecisionEngine};
use b2bua::limiter::{CallLimiter, NoopLimiter};
use b2bua::metrics::B2buaMetrics;
use b2bua::store::InMemoryCallStore;
use b2bua::{B2buaCore, B2buaDeps, ReplicationSetup};
use scenario_harness::{Agent, Dialog, Harness, RunReport};

// The canonical INVITE/180/200/ACK choreography now lives in `scenario-harness`
// (`scenario_harness::callflow`), so the single-SUT b2bua tests and the HA
// failover tests share ONE implementation of the dance. Re-exported here so
// existing `b2bua_harness::{establish, hangup, OFFER_SDP, …}` imports keep
// working — the home for the dance is `scenario_harness::callflow`.
pub use scenario_harness::callflow::{self, establish, hangup, Call, ANSWER_SDP, OFFER_SDP};
use sip_clock::Clock;
use sip_net::UdpEndpoint;
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerRegistry};
use sip_proxy::security::hmac::{HmacKey, StaticHmacKeyProvider};
use sip_proxy::{
    LoadBalancerConfig, LoadBalancerStrategy, ProxyAddr, ProxyCoreBuilder, ProxyMetrics,
    RoutingStrategy,
};
use sip_txn::IdGen;
use tokio::task::JoinHandle;

// ===========================================================================
// Shared b2bua spawn primitive
// ===========================================================================
//
// The bind→config→deps→spawn wiring for a `b2bua::B2buaCore` is identical
// between the single-SUT harness here (`B2buaSut::start_inner`) and the
// multi-node `failover-harness` (`ReplicatedB2buaSut::spawn_core`): the only
// things that vary are the `replication` value, the `clock`, the `id_gen`
// seed, the config tuning, and the `services` vec. Both bind their own
// endpoint (this crate uses `bind_sut_with_roles`, failover binds via its
// harness handle); the SHARED part is everything from "build the config" to
// "spawn the core", captured by [`B2buaSpawnParams`] + [`spawn_b2bua_core`].
// `failover-harness` reuses these (it depends on this crate).

/// Everything that varies between a single-SUT and a replicated b2bua spawn.
/// The caller binds its own endpoint and passes it (plus these params) to
/// [`spawn_b2bua_core`].
pub struct B2buaSpawnParams {
    /// `self_ordinal` (e.g. "w0" single-SUT, the worker ordinal in a cluster).
    pub ordinal: String,
    /// This worker's SIP wire address (drives `sip_local_ip`/`sip_local_port`).
    pub sip_addr: SocketAddr,
    pub decision: Arc<dyn CallDecisionEngine>,
    pub limiter: Arc<dyn CallLimiter>,
    /// Callflow services to register (ADR-0016). Empty for the plain path.
    pub services: Vec<b2bua::rules::ServiceDef>,
    pub outbound_proxy: Option<(String, u16)>,
    /// Already-built replication setup (`None` → non-replicating). The caller
    /// constructs this — keeping repl-net/topology deps out of this crate.
    pub replication: Option<ReplicationSetup>,
    pub clock: Clock,
    pub id_gen: Arc<IdGen>,
    /// CDR writer; cloned into the deps (the caller keeps its own handle to
    /// snapshot records).
    pub cdr: InMemoryCdrWriter,
}

/// Builds the base [`B2buaConfig`] (ip/port/ordinal/outbound_proxy wired),
/// applies `tune`, assembles [`B2buaDeps`], and spawns the core on the
/// already-bound `endpoint`. The single home for the spawn wiring that
/// `B2buaSut` and `failover-harness`'s `ReplicatedB2buaSut` share.
///
/// The legacy `store` slot is a throwaway [`InMemoryCallStore`]: on the
/// replicating path the repl store is the drain target (the legacy slot is
/// unused), and on the non-replicating path each call site already passed a
/// fresh in-memory store. `services` is honoured via `spawn_with_services`
/// (`spawn` is just `spawn_with_services(.., vec![])`, so an empty vec is
/// behaviour-identical to the old `spawn` path).
pub fn spawn_b2bua_core(
    endpoint: Box<dyn UdpEndpoint>,
    params: B2buaSpawnParams,
    tune: impl FnOnce(&mut B2buaConfig),
) -> B2buaCore {
    let B2buaSpawnParams {
        ordinal,
        sip_addr,
        decision,
        limiter,
        services,
        outbound_proxy,
        replication,
        clock,
        id_gen,
        cdr,
    } = params;
    let mut config = B2buaConfig {
        self_ordinal: ordinal,
        sip_local_ip: sip_addr.ip().to_string(),
        sip_local_port: sip_addr.port(),
        b2b_outbound_proxy: outbound_proxy,
        ..Default::default()
    };
    tune(&mut config);
    let deps = B2buaDeps {
        config,
        decision,
        limiter,
        cdr: Arc::new(cdr),
        store: Arc::new(InMemoryCallStore::new()),
        clock,
        id_gen,
        replication,
        metrics: B2buaMetrics::new(),
    };
    B2buaCore::spawn_with_services(endpoint, deps, services)
}

// The multi-node HA failover harness (FailoverHarness / ReplicatedB2buaSut /
// ProxySut) moved to the dedicated `failover-harness` crate (ADR-0013 §0). This
// crate is the single-SUT b2bua harness.

// ===========================================================================
// Shared load-balancing ProxyCore spawn primitive
// ===========================================================================
//
// The registry→hmac→load-observer→LoadBalancerStrategy→ProxyCoreBuilder→spawn
// wiring for a real `sip_proxy::ProxyCore` LB SUT is identical between the
// single-worker proxy helper in this crate's `tests/common/mod.rs`
// (`spawn_lb_proxy`) and the multi-worker `failover-harness`
// (`FailoverHarness::spawn_proxy_inner`): the ONLY things that vary are
// single-vs-N workers (single = a one-element slice), the clock source, and the
// failover-only health-probe task + retained registry handle. The endpoint is
// bound by each caller (this crate via `Harness::bind_sut`, failover via its
// `HarnessHandle::bind_sut`) — both yield the same `(endpoint, sock)` — so, as
// with `spawn_b2bua_core`, the caller binds and passes the bound pair in; the
// SHARED part is everything from "build the registry" to "spawn the core",
// captured by [`spawn_proxy_core`] returning [`ProxyCoreParts`]. Each caller
// wraps the parts in its own `ProxySut` (the two structs differ — the test one
// is minimal; failover's retains the registry handle for health control and
// adds the OPTIONS probe task — so only the wiring is unified, not the struct).

/// The wired-but-caller-owned pieces of a load-balancing `ProxyCore` SUT, as
/// returned by [`spawn_proxy_core`]. The caller wraps these in its own
/// `ProxySut`. The `registry` is the CONCRETE [`SimulatedWorkerRegistry`] the
/// running `ProxyCore` resolves through (a clone of the same instance), so a
/// caller that retains it can flip worker health/address and have the live proxy
/// observe the change.
pub struct ProxyCoreParts {
    /// The proxy's listen address (the bound `sock`).
    pub addr: SocketAddr,
    /// The concrete registry the live `ProxyCore` resolves through. Retain it to
    /// drive worker health/address; a probe loop can take its `control()` seam.
    pub registry: SimulatedWorkerRegistry,
    /// The proxy's metrics (the same `Arc` wired into the core).
    pub metrics: Arc<ProxyMetrics>,
    /// The AIMD load observer the `LoadBalancerStrategy` reads. Returned so a
    /// caller running a `HealthProbe` can feed the SAME instance the probe's
    /// `X-Overload` payloads (failover wires this; the single-worker test
    /// helper drops it).
    pub observer: Arc<WorkerLoadObserver>,
    /// The spawned recv-loop task. The caller's `ProxySut::drop` aborts it.
    pub task: JoinHandle<()>,
}

/// Spawn a real load-balancing `ProxyCore` on the harness fabric fronting
/// `workers` (single-worker = a one-element slice; HRW always picks the lone
/// entry). The caller binds the proxy endpoint (`Harness::bind_sut("proxy", …)`)
/// and passes the resulting `(endpoint, addr)` plus the shared `clock`; this
/// builds the `SimulatedWorkerRegistry` (all workers alive), the HMAC provider
/// (`k1`/`[7;32]`), the load observer, the `LoadBalancerStrategy`, and the core
/// (id-gen seed `0xC0FFEE`), spawns its run loop, and returns the common
/// [`ProxyCoreParts`] for the caller to wrap in its own `ProxySut`.
///
/// The health-probe task and the retained-registry health controls (the
/// failover-only extras) stay at the call site: `failover-harness` takes
/// `parts.registry.control()` for the `HealthProbe` and keeps `parts.registry`
/// in its `ProxySut` so `set_health`/`set_address` drive the live proxy.
pub fn spawn_proxy_core(
    endpoint: Box<dyn UdpEndpoint>,
    addr: SocketAddr,
    workers: &[(&str, SocketAddr)],
    clock: Clock,
) -> ProxyCoreParts {
    let entries: Vec<WorkerEntry> = workers
        .iter()
        .map(|(id, sa)| WorkerEntry::alive(*id, ProxyAddr::new(sa.ip().to_string(), sa.port())))
        .collect();
    let registry = SimulatedWorkerRegistry::with_clock(entries, clock.clone());
    let registry_dyn: Arc<dyn WorkerRegistry> = Arc::new(registry.clone());
    let hmac =
        Arc::new(StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap());
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let strategy: Arc<dyn RoutingStrategy> = Arc::new(LoadBalancerStrategy::new(
        registry_dyn.clone(),
        hmac,
        observer.clone(),
        Arc::new(ProxyMetrics::new()),
        clock.clone(),
        LoadBalancerConfig::default(),
    ));

    let metrics = Arc::new(ProxyMetrics::new());
    let core = ProxyCoreBuilder::new(ProxyAddr::from(addr), strategy, registry_dyn)
        .clock(clock)
        .id_gen(Arc::new(IdGen::seeded(0xC0FFEE)))
        .metrics(metrics.clone())
        .build(endpoint);
    let task = tokio::spawn(core.run());
    ProxyCoreParts { addr, registry, metrics, observer, task }
}

/// Poll `cond` until it holds, yielding so spawned teardown / CDR-writer tasks
/// drain. The single home for the "wait for the async teardown to land" loop
/// every e2e test used to hand-roll.
///
/// Works under both wall-clock (`#[tokio::test]`) and `start_paused` tests:
/// under a paused runtime the `sleep` auto-advances virtual time, so the
/// awaited spawned tasks still get to run. Bounded (200 × 5 ms) so a real
/// regression — `cond` never holds — falls through to the test's assertions
/// instead of hanging.
///
/// For tests that must *advance the clock to trip a timer* (not merely drain
/// already-due tasks), advance with `Harness::advance` first, then settle here.
pub async fn settle_until(cond: impl Fn() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// A running B2BUA bound on the harness fabric. Keep it alive for the duration
/// of the scenario (drop tears the worker tasks down with the endpoint).
pub struct B2buaSut {
    pub addr: SocketAddr,
    cdr: InMemoryCdrWriter,
    metrics: B2buaMetrics,
    _core: B2buaCore,
}

/// Composable builder for a single-SUT [`B2buaSut`] (slice 3). Replaces the
/// former fan of `start*` / `route_all*` constructors: the decision engine is
/// fixed at construction (via [`B2buaSut::builder`] or one of the `route_all*`
/// convenience constructors that pre-build a common decision engine), and every
/// other axis — outbound proxy, limiter, callflow services, config tuning — is
/// an optional chain method. [`start`](Self::start) is the single terminal that
/// binds and spawns the core, preserving the exact wiring the old `start_inner`
/// had (binds `{Uac, Uas}` roles; keepalive defaults 30/5 applied FIRST then the
/// caller `tune` LAST so a test overriding keepalive still wins; `NoopLimiter`,
/// empty services, no replication, `Clock::test_at(0)`, id-gen seed `0xB2B0`,
/// ordinal `"w0"` defaults).
pub struct B2buaSutBuilder {
    decision: Arc<dyn CallDecisionEngine>,
    outbound_proxy: Option<(String, u16)>,
    limiter: Arc<dyn CallLimiter>,
    services: Vec<b2bua::rules::ServiceDef>,
    tune: Box<dyn FnOnce(&mut B2buaConfig)>,
}

impl B2buaSutBuilder {
    /// Route the b-leg through the front proxy at `(host, port)` (the
    /// `alice → proxy → b2bua → proxy → bob` topology; see
    /// [`B2buaConfig::b2b_outbound_proxy`]).
    pub fn outbound_proxy(mut self, host: &str, port: u16) -> Self {
        self.outbound_proxy = Some((host.to_string(), port));
        self
    }

    /// Use a custom [`CallLimiter`] (e.g. an `HttpCallLimiter` over the
    /// simulated HTTP fabric serving a real `LimiterServer`). Defaults to
    /// `NoopLimiter`.
    pub fn limiter(mut self, limiter: Arc<dyn CallLimiter>) -> Self {
        self.limiter = limiter;
        self
    }

    /// Register a set of callflow services (ADR-0016): each service's `init`
    /// runs at setup and its state-gated rules compose above the core defaults.
    pub fn services(mut self, services: Vec<b2bua::rules::ServiceDef>) -> Self {
        self.services = services;
        self
    }

    /// Config mutator hook (the faithful equivalent of a per-scenario
    /// `configOverrides`). Runs LAST in [`start`](Self::start) — after the
    /// harness keepalive defaults — so a test may still override them.
    pub fn tune(mut self, tune: impl FnOnce(&mut B2buaConfig) + 'static) -> Self {
        self.tune = Box::new(tune);
        self
    }

    /// Bind the B2BUA at `addr` and spawn its core, consuming the builder.
    pub async fn start(self, h: &Harness, name: &str, addr: &str) -> B2buaSut {
        let B2buaSutBuilder { decision, outbound_proxy, limiter, services, tune } = self;
        // The B2BUA terminates each leg as a UA (UAS on the a-leg, UAC on the
        // b-leg) — it is NOT an RFC 3261 §16 proxy, so its bind declares
        // `{Uac, Uas}` and the proxy-subject audit rules (no-target-404,
        // 100-within-200ms, unmatched-PRACK forwarding, strict-route rewrite)
        // do not judge this lane.
        let (endpoint, sa) = h
            .bind_sut_with_roles(
                name,
                addr,
                std::collections::HashSet::from([sip_net::UaRole::Uac, sip_net::UaRole::Uas]),
            )
            .await;
        let cdr = InMemoryCdrWriter::new();
        let params = B2buaSpawnParams {
            ordinal: "w0".into(),
            sip_addr: sa,
            decision,
            limiter,
            services,
            outbound_proxy,
            replication: None,
            clock: Clock::test_at(0),
            id_gen: Arc::new(IdGen::seeded(0xB2B0)),
            cdr: cdr.clone(),
        };
        let core = spawn_b2bua_core(endpoint, params, |config| {
            // Production default is 300 s (5 min); the paused-clock keepalive
            // tests advance in 30 s steps, so the harness baseline stays at 30 s.
            // A scenario can still override via `tune`.
            config.keepalive_interval_sec = 30;
            // Production default is now 32 s; the paused-clock keepalive-timeout
            // tests advance a fixed 5 s after the probe, so the harness pins the
            // old 5 s grace to keep those steps valid (a scenario can `tune` it).
            config.keepalive_timeout_sec = 5;
            // The caller's tune runs LAST so it can still override the keepalive
            // defaults above (preserving the prior ordering).
            tune(config);
        });
        let metrics = core.metrics().clone();
        B2buaSut {
            addr: sa,
            cdr,
            metrics,
            _core: core,
        }
    }
}

impl B2buaSut {
    /// Base builder: a B2BUA driven by `decision`, every other axis at its
    /// default (no outbound proxy, `NoopLimiter`, no services, no-op tune).
    pub fn builder(decision: Arc<dyn CallDecisionEngine>) -> B2buaSutBuilder {
        B2buaSutBuilder {
            decision,
            outbound_proxy: None,
            limiter: Arc::new(NoopLimiter),
            services: Vec::new(),
            tune: Box::new(|_| {}),
        }
    }

    /// Builder for a B2BUA that routes every call to `dest` (the common case).
    pub fn route_all_to(dest_host: &str, dest_port: u16) -> B2buaSutBuilder {
        Self::builder(Arc::new(ScriptedDecisionEngine::route_all_to(dest_host, dest_port)))
    }

    /// Builder for a B2BUA that routes every call to `dest` and authorizes REFER
    /// transfers via the default `X-Api-Call`-keyed `/call/refer` behavior (the
    /// REFER-scenario constructor).
    pub fn route_all_with_refer(dest_host: &str, dest_port: u16) -> B2buaSutBuilder {
        Self::builder(Arc::new(ScriptedDecisionEngine::route_all_with_refer(
            dest_host, dest_port,
        )))
    }

    /// Builder for a B2BUA that routes every call to `dest` with the
    /// `relayFirst18xTo180` feature active under `strategy` (suppress / fake-prack).
    pub fn route_all_to_with_18x(
        dest_host: &str,
        dest_port: u16,
        strategy: call::features::RelayFirst18xStrategy,
    ) -> B2buaSutBuilder {
        let dest = (dest_host.to_string(), dest_port);
        Self::builder(Arc::new(
            b2bua::decision::ScriptedDecisionEngine::builder()
                .fallback(move |_req| {
                    b2bua::decision::NewCallResponse::Route(
                        b2bua::decision::test_adapter::route_to_with_18x(
                            &dest.0, dest.1, strategy,
                        ),
                    )
                })
                .build(),
        ))
    }

    /// Builder for a B2BUA that routes the call to `dest_port` (bob1) with the
    /// `relayFirst18xTo180` feature active under `strategy` and a `callback_context`
    /// set (the failover-capable marker), and fails over via `/call/failure` to
    /// `failover_port` (bob2) with `failover_ruri` as the new Request-URI. Mirrors
    /// the TS `on_failure: { action: "failover", destination, new_ruri }` instruction.
    pub fn route_all_to_with_18x_failover(
        dest_host: &str,
        dest_port: u16,
        failover_port: u16,
        failover_ruri: &str,
        strategy: call::features::RelayFirst18xStrategy,
    ) -> B2buaSutBuilder {
        use b2bua::decision::test_adapter::route_to_with_18x;
        use b2bua::decision::{CallFailureResponse, NewCallResponse};
        let primary = (dest_host.to_string(), dest_port);
        let failover = (dest_host.to_string(), failover_port);
        let failover_ruri = failover_ruri.to_string();
        Self::builder(Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(move |_req| {
                    let mut r = route_to_with_18x(&primary.0, primary.1, strategy);
                    r.callback_context = Some("failover-test".into());
                    // The TS relies on the 30 s platform no-answer default the Rust
                    // default route omits — set it so the no-answer failover fires.
                    r.no_answer_timeout_sec = Some(30);
                    NewCallResponse::Route(r)
                })
                .on_failure(move |_req| {
                    let mut r = route_to_with_18x(&failover.0, failover.1, strategy);
                    r.new_ruri = Some(failover_ruri.clone());
                    CallFailureResponse::Route(r)
                })
                .build(),
        ))
    }

    pub fn cdr_records(&self) -> Vec<CdrRecord> {
        self.cdr.snapshot()
    }

    pub fn metrics(&self) -> &B2buaMetrics {
        &self.metrics
    }

    /// Ground-truth live call-map size (`inner.calls.len()`). An orphan-reject
    /// (481) does NOT inflate this — it never inserts a call — so it is the wrong
    /// lens for the orphan leak; use [`lock_count`](Self::lock_count) +
    /// `metrics().creations_total()/removals_total()` for that.
    pub fn active_calls(&self) -> usize {
        self._core.active_calls()
    }

    /// Live per-call serialization-lock count. Should return to 0 once traffic
    /// drains; a residue is the orphan-reject lock leak (one stranded lock per
    /// in-dialog request that 481'd without tearing its per-call state down).
    pub fn lock_count(&self) -> usize {
        self._core.lock_count()
    }

    /// The named "no leak" oracle: every call created has been reaped and no
    /// per-call state survives. Asserts the three invariants the reap tests
    /// used to spell out by hand:
    ///   1. `creations_total() == removals_total()` — every call's lifecycle
    ///      closed (the `active_calls` lens misses orphan-reject leaks, which
    ///      never insert a call; the paired counters + `lock_count` catch them).
    ///   2. `active_calls() == 0` — no live call left in the map.
    ///   3. `lock_count() == 0` — no stranded per-call serialization lock.
    ///
    /// A new leak dimension (e.g. the `b2bua_timer_queue_len − b2bua_timer_live`
    /// tombstone gap from the CLAUDE.md timer hazard) is added here once and is
    /// then checked by every reap test — the locality this oracle exists for.
    ///
    /// Call it *after* the teardown has drained (see [`settle_until`]).
    #[track_caller]
    pub fn assert_fully_reaped(&self) {
        let (creations, removals) = (
            self.metrics.creations_total(),
            self.metrics.removals_total(),
        );
        assert_eq!(
            creations, removals,
            "call leak: creations ({creations}) != removals ({removals}) — a call \
             was created but never reaped"
        );
        assert_eq!(
            self.active_calls(),
            0,
            "call leak: {} live call(s) left in the map",
            self.active_calls()
        );
        assert_eq!(
            self.lock_count(),
            0,
            "lock leak: {} stranded per-call serialization lock(s)",
            self.lock_count()
        );
        // 4. (ADR-0020) the reaper's last-touched ledger mirrors the call map;
        //    a residue is a stamp leak.
        assert_eq!(
            self._core.touched_count(),
            0,
            "stamp leak: {} stranded last-touched ledger entr(ies)",
            self._core.touched_count()
        );
    }
}

/// Canonical default port for `alice` in a [`B2buaScene`].
pub const ALICE_PORT: u16 = 5060;
/// Canonical default port for `bob` in a [`B2buaScene`].
pub const BOB_PORT: u16 = 5070;
/// Canonical default port for the `b2bua` SUT in a [`B2buaScene`].
pub const B2BUA_PORT: u16 = 5080;

/// The standard alice ↔ b2bua ↔ bob fixture with canonical default ports and a
/// b2bua that routes every call to bob — so a new test never picks ports or
/// wires routing by hand. Access [`alice`](Self::alice)/[`bob`](Self::bob)/
/// [`b2bua`](Self::b2bua) for the interesting part; [`establish`](Self::establish)
/// runs the canonical call (delegating to [`callflow::establish`]);
/// [`finish`](Self::finish) gates RFC + renders.
///
/// # Writing a new single-SUT b2bua test
///
/// The 90% case is one line:
///
/// ```ignore
/// let s = B2buaScene::new("my-test").await;
/// let mut dialog = s.establish().await;          // alice → b2bua → bob, confirmed
/// // ... the interesting part, driving `s.alice` / `s.bob` / `s.b2bua` ...
/// s.hangup(&mut dialog).await;                    // BYE / 200
/// s.finish().await;                              // RFC gate + render
/// ```
///
/// Each `Harness` has its own simulated network namespace, so the fixed ports
/// never collide between tests. For a non-default b2bua decision (refer, limiter,
/// services, config tune) build it via [`with_b2bua`](Self::with_b2bua), which
/// hands you bob's port and the full [`B2buaSutBuilder`]:
///
/// ```ignore
/// let s = B2buaScene::with_b2bua("refer-test", |bob_port| {
///     B2buaSut::route_all_with_refer("127.0.0.1", bob_port)
/// }).await;
/// ```
pub struct B2buaScene {
    pub h: Harness,
    pub alice: Agent,
    pub bob: Agent,
    pub b2bua: B2buaSut,
}

impl B2buaScene {
    /// alice/bob/b2bua bound at the canonical default ports
    /// ([`ALICE_PORT`]/[`BOB_PORT`]/[`B2BUA_PORT`]); the b2bua routes every call
    /// to bob. The 90%-case fixture — one line for a new test.
    pub async fn new(name: &str) -> Self {
        Self::with_b2bua(name, |bob_port| B2buaSut::route_all_to("127.0.0.1", bob_port)).await
    }

    /// Like [`new`](Self::new) but customise the b2bua: `build` is handed bob's
    /// canonical port and must return the [`B2buaSutBuilder`] to start (start is
    /// done for you at [`B2BUA_PORT`]). This composes cleanly with every existing
    /// `B2buaSut::route_all*` entry point and the builder's chain methods
    /// (`outbound_proxy`, `limiter`, `services`, `tune`), so a test needing a
    /// non-default decision still gets the canonical alice/bob/ports wiring for
    /// free. Default decision = `route_all_to(bob)` (see [`new`](Self::new)).
    pub async fn with_b2bua(
        name: &str,
        build: impl FnOnce(u16) -> B2buaSutBuilder,
    ) -> Self {
        // Match the common b2bua-test convention: the harness's default
        // SIMULATED_TRANSIT_DELAY_MS (1 ms floor enforced by the fabric).
        let h = Harness::new(name);
        let alice = h.agent("alice", &format!("127.0.0.1:{ALICE_PORT}")).await;
        let bob = h.agent("bob", &format!("127.0.0.1:{BOB_PORT}")).await;
        let b2bua = build(BOB_PORT)
            .start(&h, "b2bua", &format!("127.0.0.1:{B2BUA_PORT}"))
            .await;
        Self { h, alice, bob, b2bua }
    }

    /// Run the canonical call (alice → b2bua → bob, confirmed) and return alice's
    /// [`Dialog`]. Equivalent to
    /// `callflow::establish(&self.alice, &self.bob, self.b2bua.addr)`.
    pub async fn establish(&self) -> Dialog {
        callflow::establish(&self.alice, &self.bob, self.b2bua.addr).await
    }

    /// Tear the call down (alice BYE / bob 200). Equivalent to
    /// `callflow::hangup(dialog, &self.bob)`.
    pub async fn hangup(&self, dialog: &mut Dialog) {
        callflow::hangup(dialog, &self.bob).await
    }

    /// Gate RFC audit + render the report. Consumes the scene (`Harness::finish`
    /// consumes `self`).
    pub async fn finish(self) -> RunReport {
        self.h.finish().await
    }
}
