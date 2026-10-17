//! The recv shards' shared routing state, and each shard's core built over it.

use std::sync::Arc;

use sip_clock::Clock;
use sip_proxy::cancel_lru::CancelBranchLru;
use sip_proxy::health::FirstView;
use sip_proxy::liveness::ShardPulse;
use sip_proxy::refusals::Refusals;
use sip_proxy::registry::WorkerRegistry;
use sip_proxy::resolver::ResolverConfig;
use sip_proxy::self_gate::{IntakeAgeRecorder, ProxySelfGate};
use sip_proxy::{ProxyAddr, ProxyCoreBuilder, ProxyMetrics, ProxyTraces, RoutingStrategy};
use sip_txn::IdGen;

/// What every recv shard shares. Kernel flow hashing keeps one source on one
/// shard, but a response, a CANCEL or a copy can reach another socket than
/// the request it follows, so every memo of a request is ONE instance here.
pub struct SharedRouting {
    pub advertised: ProxyAddr,
    pub strategy: Arc<dyn RoutingStrategy>,
    pub registry: Arc<dyn WorkerRegistry>,
    pub clock: Clock,
    pub id_gen: Arc<IdGen>,
    pub metrics: Arc<ProxyMetrics>,
    pub cancel_lru: Arc<CancelBranchLru>,
    pub refusals: Arc<Refusals>,
    pub self_gate: Arc<dyn ProxySelfGate>,
    pub intake_age: IntakeAgeRecorder,
    pub resolver: ResolverConfig,
    pub traces: Arc<ProxyTraces>,
    pub pulse: Arc<ShardPulse>,
    /// The proxy's first worker view, which its health probe latches.
    pub first_view: Arc<FirstView>,
}

impl SharedRouting {
    /// The builder of recv shard `shard`'s core over the shared state.
    pub fn shard(&self, shard: usize) -> ProxyCoreBuilder {
        ProxyCoreBuilder::new(self.advertised.clone(), self.strategy.clone(), self.registry.clone())
            .clock(self.clock.clone())
            .id_gen(self.id_gen.clone())
            .metrics(self.metrics.clone())
            .cancel_lru(self.cancel_lru.clone())
            .refusals(self.refusals.clone())
            .self_gate(self.self_gate.clone())
            .intake_age(self.intake_age.clone())
            .resolver_config(self.resolver)
            .traces(self.traces.clone())
            .pulse(self.pulse.clone())
            .first_view(self.first_view.clone())
            .shard(shard)
    }
}

#[cfg(test)]
mod tests;
