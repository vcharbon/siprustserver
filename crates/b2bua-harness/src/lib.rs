//! Test-only harness: binds a real [`b2bua::B2buaCore`] as a System-Under-Test
//! on the `scenario-harness` simulated network, so deterministic
//! alice ↔ b2bua ↔ bob flows run end-to-end through the recording. Extends the
//! `bind_sut` seam (ADR-0006/0009) to the B2BUA (ADR-0010).

use std::net::SocketAddr;
use std::sync::Arc;

use b2bua::cdr::{CdrRecord, InMemoryCdrWriter};
use b2bua::config::B2buaConfig;
use b2bua::decision::{CallDecisionEngine, ScriptedDecisionEngine};
use b2bua::limiter::NoopLimiter;
use b2bua::metrics::B2buaMetrics;
use b2bua::store::InMemoryCallStore;
use b2bua::{B2buaCore, B2buaDeps};
use scenario_harness::Harness;
use sip_clock::Clock;
use sip_txn::IdGen;

mod failover;

pub use failover::{FailoverHarness, ProxySut, ReplicatedB2buaSut};

// Re-export the engine value types the failover tests touch so a consumer needs
// only this crate (+ scenario-harness) for the canonical scenario.
pub use b2bua::store::PartitionRole;
pub use sip_proxy::registry::WorkerHealth;

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
        let (endpoint, sa) = h.bind_sut(name, addr).await;
        let cdr = InMemoryCdrWriter::new();
        let config = B2buaConfig {
            self_ordinal: "w0".into(),
            sip_local_ip: sa.ip().to_string(),
            sip_local_port: sa.port(),
            b2b_outbound_proxy: outbound_proxy,
            ..Default::default()
        };
        let deps = B2buaDeps {
            config,
            decision,
            limiter: Arc::new(NoopLimiter),
            cdr: Arc::new(cdr.clone()),
            store: Arc::new(InMemoryCallStore::new()),
            clock: Clock::test_at(0),
            id_gen: Arc::new(IdGen::seeded(0xB2B0)),
            replication: None,
        };
        let core = B2buaCore::spawn(endpoint, deps);
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
}
