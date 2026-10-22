//! The recv shards' shared routing state, and what is built over it: each
//! shard's core, the health probe and the probe routes, all on ONE first
//! worker view (ADR-0050 §11).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use sip_clock::Clock;
use sip_net::UdpEndpoint;
use sip_proxy::cancel_lru::CancelBranchLru;
use sip_proxy::health::{FirstView, HealthProbe, HealthProbeConfig};
use sip_proxy::liveness::ShardPulse;
use sip_proxy::load_observer::WorkerLoadObserver;
use sip_proxy::parse_refusals::ParseRefusals;
use sip_proxy::refusals::Refusals;
use sip_proxy::registry::control::WorkerRegistryControl;
use sip_proxy::registry::WorkerRegistry;
use sip_proxy::resolver::ResolverConfig;
use sip_proxy::self_gate::{IntakeAgeRecorder, ProxySelfGate};
use sip_proxy::{ProxyAddr, ProxyCoreBuilder, ProxyMetrics, ProxyTraces, RoutingStrategy};
use sip_txn::IdGen;

use crate::readiness::{probes, ProxyProbes};

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
    pub parse_refusals: Arc<ParseRefusals>,
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
        ProxyCoreBuilder::new(
            self.advertised.clone(),
            self.strategy.clone(),
            self.registry.clone(),
            self.id_gen.clone(),
        )
        .clock(self.clock.clone())
        .metrics(self.metrics.clone())
        .cancel_lru(self.cancel_lru.clone())
        .refusals(self.refusals.clone())
        .parse_refusals(self.parse_refusals.clone())
        .self_gate(self.self_gate.clone())
        .intake_age(self.intake_age.clone())
        .resolver_config(self.resolver)
        .traces(self.traces.clone())
        .pulse(self.pulse.clone())
        .first_view(self.first_view.clone())
        .shard(shard)
    }

    /// The OPTIONS health probe over the worker pool on `endpoint`, writing
    /// health through `control` and load payloads into `observer`; it latches
    /// the first view the shards and the probe routes read. Its misses are
    /// counted per peer (`sip_proxy_peer_failures_total`).
    pub fn health_probe(
        &self,
        endpoint: Box<dyn UdpEndpoint>,
        control: Arc<dyn WorkerRegistryControl>,
        observer: Arc<WorkerLoadObserver>,
        config: HealthProbeConfig,
    ) -> HealthProbe {
        HealthProbe::new(
            endpoint,
            self.registry.clone(),
            control,
            observer,
            self.clock.clone(),
            self.id_gen.clone(),
            config,
        )
        .with_metrics(self.metrics.clone())
        .with_first_view(self.first_view.clone())
    }

    /// The probe routes, read from the shards' pulse and the first view; a
    /// shard stalled past `stall_ms` fails them all, `draining` fails `/readyz`.
    pub fn probes(&self, draining: Arc<AtomicBool>, stall_ms: u64) -> ProxyProbes {
        probes(
            draining,
            self.pulse.clone(),
            self.clock.clone(),
            stall_ms,
            self.registry.clone(),
            self.first_view.clone(),
        )
    }
}

#[cfg(test)]
mod tests;
