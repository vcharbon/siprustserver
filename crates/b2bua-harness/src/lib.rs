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
use b2bua::{B2buaCore, B2buaDeps};
use scenario_harness::Harness;
use sip_clock::Clock;
use sip_txn::IdGen;

// The multi-node HA failover harness (FailoverHarness / ReplicatedB2buaSut /
// ProxySut) moved to the dedicated `failover-harness` crate (ADR-0013 §0). This
// crate is the single-SUT b2bua harness.

/// A running B2BUA bound on the harness fabric. Keep it alive for the duration
/// of the scenario (drop tears the worker tasks down with the endpoint).
pub struct B2buaSut {
    pub addr: SocketAddr,
    cdr: InMemoryCdrWriter,
    metrics: B2buaMetrics,
    _core: B2buaCore,
}

impl B2buaSut {
    /// Bind a B2BUA at `addr` driven by `decision`.
    pub async fn start(
        h: &Harness,
        name: &str,
        addr: &str,
        decision: Arc<dyn CallDecisionEngine>,
    ) -> Self {
        Self::start_with_outbound_proxy(h, name, addr, decision, None).await
    }

    /// Bind a B2BUA at `addr` driven by `decision`, optionally deployed behind
    /// the front proxy: when `outbound_proxy` is `Some((host, port))`, every
    /// b-leg outbound request traverses that proxy (see
    /// [`B2buaConfig::b2b_outbound_proxy`]).
    pub async fn start_with_outbound_proxy(
        h: &Harness,
        name: &str,
        addr: &str,
        decision: Arc<dyn CallDecisionEngine>,
        outbound_proxy: Option<(String, u16)>,
    ) -> Self {
        Self::start_with_config(h, name, addr, decision, outbound_proxy, |_| {}).await
    }

    /// Bind a B2BUA with a config mutator hook. The base config is the default
    /// (with `self_ordinal`/local IP+port/outbound-proxy wired); `tune` may
    /// override any other field (the faithful equivalent of a per-scenario
    /// `configOverrides` — the source stack applies them worker-wide too).
    pub async fn start_with_config(
        h: &Harness,
        name: &str,
        addr: &str,
        decision: Arc<dyn CallDecisionEngine>,
        outbound_proxy: Option<(String, u16)>,
        tune: impl FnOnce(&mut B2buaConfig),
    ) -> Self {
        Self::start_inner(h, name, addr, decision, outbound_proxy, Arc::new(NoopLimiter), Vec::new(), tune).await
    }

    /// Bind a B2BUA with a set of registered callflow services (ADR-0016): each
    /// service's `init` runs at setup and its state-gated rules compose above the
    /// core defaults. Used by the out-of-tree `announcement` capstone (slice 8) —
    /// the service is injected here, so `b2bua` never depends on it.
    pub async fn start_with_services(
        h: &Harness,
        name: &str,
        addr: &str,
        decision: Arc<dyn CallDecisionEngine>,
        services: Vec<b2bua::rules::ServiceDef>,
    ) -> Self {
        Self::start_inner(h, name, addr, decision, None, Arc::new(NoopLimiter), services, |_| {}).await
    }

    /// Bind a B2BUA with a custom [`CallLimiter`] (e.g. an `HttpCallLimiter` over
    /// the simulated HTTP fabric serving a real `LimiterServer`). No outbound
    /// proxy; `tune` may override config (e.g. `limiter_refresh_sec`,
    /// `self_ordinal` for the cross-worker scenario).
    pub async fn start_with_limiter(
        h: &Harness,
        name: &str,
        addr: &str,
        decision: Arc<dyn CallDecisionEngine>,
        limiter: Arc<dyn CallLimiter>,
        tune: impl FnOnce(&mut B2buaConfig),
    ) -> Self {
        Self::start_inner(h, name, addr, decision, None, limiter, Vec::new(), tune).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_inner(
        h: &Harness,
        name: &str,
        addr: &str,
        decision: Arc<dyn CallDecisionEngine>,
        outbound_proxy: Option<(String, u16)>,
        limiter: Arc<dyn CallLimiter>,
        services: Vec<b2bua::rules::ServiceDef>,
        tune: impl FnOnce(&mut B2buaConfig),
    ) -> Self {
        let (endpoint, sa) = h.bind_sut(name, addr).await;
        let cdr = InMemoryCdrWriter::new();
        let mut config = B2buaConfig {
            self_ordinal: "w0".into(),
            sip_local_ip: sa.ip().to_string(),
            sip_local_port: sa.port(),
            b2b_outbound_proxy: outbound_proxy,
            // Production default is 300 s (5 min); the paused-clock keepalive
            // tests advance in 30 s steps, so the harness baseline stays at 30 s.
            // A scenario can still override via `tune`.
            keepalive_interval_sec: 30,
            // Production default is now 32 s; the paused-clock keepalive-timeout
            // tests advance a fixed 5 s after the probe, so the harness pins the
            // old 5 s grace to keep those steps valid (a scenario can `tune` it).
            keepalive_timeout_sec: 5,
            ..Default::default()
        };
        tune(&mut config);
        let deps = B2buaDeps {
            config,
            decision,
            limiter,
            cdr: Arc::new(cdr.clone()),
            store: Arc::new(InMemoryCallStore::new()),
            clock: Clock::test_at(0),
            id_gen: Arc::new(IdGen::seeded(0xB2B0)),
            replication: None,
        };
        let core = B2buaCore::spawn_with_services(endpoint, deps, services);
        let metrics = core.metrics().clone();
        Self {
            addr: sa,
            cdr,
            metrics,
            _core: core,
        }
    }

    /// Bind a B2BUA that routes every call to `dest` (the common case).
    pub async fn route_all_to(
        h: &Harness,
        name: &str,
        addr: &str,
        dest_host: &str,
        dest_port: u16,
    ) -> Self {
        let decision = Arc::new(ScriptedDecisionEngine::route_all_to(dest_host, dest_port));
        Self::start(h, name, addr, decision).await
    }

    /// Bind a B2BUA that routes every call to `dest` and authorizes REFER
    /// transfers via the default `X-Api-Call`-keyed `/call/refer` behavior (the
    /// REFER-scenario constructor).
    pub async fn route_all_with_refer(
        h: &Harness,
        name: &str,
        addr: &str,
        dest_host: &str,
        dest_port: u16,
    ) -> Self {
        let decision = Arc::new(ScriptedDecisionEngine::route_all_with_refer(dest_host, dest_port));
        Self::start(h, name, addr, decision).await
    }

    /// Like [`route_all_with_refer`](Self::route_all_with_refer) but with the
    /// REFER realignment / overall-safety timers overridden (the per-scenario
    /// `configOverrides` the `refer-timers` corpus uses: push
    /// `refer_reinvite_answer` out past `refer_overall_safety` so the overall
    /// watchdog trips first while a realign re-INVITE is stuck unanswered).
    pub async fn route_all_with_refer_timers(
        h: &Harness,
        name: &str,
        addr: &str,
        dest_host: &str,
        dest_port: u16,
        refer_reinvite_answer_sec: i64,
        refer_overall_safety_sec: i64,
    ) -> Self {
        let decision = Arc::new(ScriptedDecisionEngine::route_all_with_refer(dest_host, dest_port));
        Self::start_with_config(h, name, addr, decision, None, move |c| {
            c.refer_reinvite_answer_sec = refer_reinvite_answer_sec;
            c.refer_overall_safety_sec = refer_overall_safety_sec;
        })
        .await
    }

    /// Bind a B2BUA that routes every call to `dest` with the
    /// `relayFirst18xTo180` feature active under `strategy` (suppress / fake-prack).
    pub async fn route_all_to_with_18x(
        h: &Harness,
        name: &str,
        addr: &str,
        dest_host: &str,
        dest_port: u16,
        strategy: call::features::RelayFirst18xStrategy,
    ) -> Self {
        let dest = (dest_host.to_string(), dest_port);
        let decision = Arc::new(
            b2bua::decision::ScriptedDecisionEngine::builder()
                .fallback(move |_req| {
                    b2bua::decision::NewCallResponse::Route(
                        b2bua::decision::test_adapter::route_to_with_18x(
                            &dest.0, dest.1, strategy,
                        ),
                    )
                })
                .build(),
        );
        Self::start(h, name, addr, decision).await
    }

    /// Bind a B2BUA that routes the call to `dest_port` (bob1) with the
    /// `relayFirst18xTo180` feature active under `strategy` and a `callback_context`
    /// set (the failover-capable marker), and fails over via `/call/failure` to
    /// `failover_port` (bob2) with `failover_ruri` as the new Request-URI. Mirrors
    /// the TS `on_failure: { action: "failover", destination, new_ruri }` instruction.
    #[allow(clippy::too_many_arguments)]
    pub async fn route_all_to_with_18x_failover(
        h: &Harness,
        name: &str,
        addr: &str,
        dest_host: &str,
        dest_port: u16,
        failover_port: u16,
        failover_ruri: &str,
        strategy: call::features::RelayFirst18xStrategy,
    ) -> Self {
        use b2bua::decision::{CallFailureResponse, NewCallResponse};
        use b2bua::decision::test_adapter::route_to_with_18x;
        let primary = (dest_host.to_string(), dest_port);
        let failover = (dest_host.to_string(), failover_port);
        let failover_ruri = failover_ruri.to_string();
        let decision = Arc::new(
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
                    CallFailureResponse::Failover(r)
                })
                .build(),
        );
        Self::start(h, name, addr, decision).await
    }

    /// Bind a B2BUA that routes every call to `dest` but sends its b-leg
    /// (worker→callee) traffic through the front proxy at `proxy` — the
    /// `alice → proxy → b2bua → proxy → bob` topology.
    pub async fn route_all_to_via_proxy(
        h: &Harness,
        name: &str,
        addr: &str,
        dest_host: &str,
        dest_port: u16,
        proxy_host: &str,
        proxy_port: u16,
    ) -> Self {
        let decision = Arc::new(ScriptedDecisionEngine::route_all_to(dest_host, dest_port));
        Self::start_with_outbound_proxy(
            h,
            name,
            addr,
            decision,
            Some((proxy_host.to_string(), proxy_port)),
        )
        .await
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
}
