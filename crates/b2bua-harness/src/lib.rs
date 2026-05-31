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
        let (endpoint, sa) = h.bind_sut(name, addr).await;
        let cdr = InMemoryCdrWriter::new();
        let config = B2buaConfig {
            self_ordinal: "w0".into(),
            sip_local_ip: sa.ip().to_string(),
            sip_local_port: sa.port(),
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

    pub fn cdr_records(&self) -> Vec<CdrRecord> {
        self.cdr.snapshot()
    }

    pub fn metrics(&self) -> &B2buaMetrics {
        &self.metrics
    }
}
