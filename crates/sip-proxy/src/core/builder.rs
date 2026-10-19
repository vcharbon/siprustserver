//! [`ProxyCoreBuilder`] — the one place the defaults of a [`ProxyCore`] live.

use std::sync::Arc;

use sip_clock::Clock;
use sip_net::UdpEndpoint;
use sip_txn::IdGen;

use crate::addr::ProxyAddr;
use crate::cancel_lru::CancelBranchLru;
use crate::health::FirstView;
use crate::liveness::ShardPulse;
use crate::observability::ProxyMetrics;
use crate::refusals::Refusals;
use crate::registry::WorkerRegistry;
use crate::resolver::{HostResolver, ResolverConfig, SystemResolver};
use crate::self_gate::{AlwaysAdmitGate, IntakeAgeRecorder, ProxySelfGate};
use crate::strategy::RoutingStrategy;
use crate::trace::ProxyTraces;

use super::{ExternalFaceParts, ProxyCore, ProxyCoreParts};

/// Convenience builder for the common all-defaults wiring (AlwaysAdmitGate +
/// fresh metrics + system clock) around the caller's identifier generator.
pub struct ProxyCoreBuilder {
    advertised: ProxyAddr,
    external: Option<ExternalFaceParts>,
    strategy: Arc<dyn RoutingStrategy>,
    registry: Arc<dyn WorkerRegistry>,
    cancel_lru: Option<Arc<CancelBranchLru>>,
    id_gen: Arc<IdGen>,
    refusals: Option<Arc<Refusals>>,
    clock: Option<Clock>,
    metrics: Option<Arc<ProxyMetrics>>,
    self_gate: Option<Arc<dyn ProxySelfGate>>,
    intake_age: Option<IntakeAgeRecorder>,
    resolver: Option<Arc<dyn HostResolver>>,
    resolver_cfg: Option<ResolverConfig>,
    shard: usize,
    traces: Option<Arc<ProxyTraces>>,
    pulse: Option<Arc<ShardPulse>>,
    first_view: Option<Arc<FirstView>>,
}

impl ProxyCoreBuilder {
    /// The builder of a core advertising `advertised`, routing by `strategy`
    /// over `registry`; `id_gen` keys the default [`Refusals`]' To-tag secret.
    pub fn new(
        advertised: ProxyAddr,
        strategy: Arc<dyn RoutingStrategy>,
        registry: Arc<dyn WorkerRegistry>,
        id_gen: Arc<IdGen>,
    ) -> Self {
        Self {
            advertised,
            external: None,
            strategy,
            registry,
            cancel_lru: None,
            id_gen,
            refusals: None,
            clock: None,
            metrics: None,
            self_gate: None,
            intake_age: None,
            resolver: None,
            resolver_cfg: None,
            shard: 0,
            traces: None,
            pulse: None,
            first_view: None,
        }
    }

    /// [`new`](Self::new) for this crate's unit tests: identifiers drawn
    /// deterministically.
    #[cfg(test)]
    pub(crate) fn for_test(
        advertised: ProxyAddr,
        strategy: Arc<dyn RoutingStrategy>,
        registry: Arc<dyn WorkerRegistry>,
    ) -> Self {
        Self::new(advertised, strategy, registry, Arc::new(IdGen::seeded(0xC0FFEE)))
    }

    pub fn clock(mut self, clock: Clock) -> Self {
        self.clock = Some(clock);
        self
    }
    /// Enable **dual-face mode**: a second socket on the external (caller)
    /// plane with its own advertise, and the internal CIDR list that picks the
    /// egress face per destination IP. Absent → single-face.
    pub fn external_face(mut self, parts: ExternalFaceParts) -> Self {
        self.external = Some(parts);
        self
    }
    /// The answers to refused requests. Pass ONE to every recv shard so a copy
    /// arriving on another socket draws the same answer; the default is a
    /// private one with no `Retry-After` jitter, its secret drawn from the
    /// builder's generator.
    pub fn refusals(mut self, refusals: Arc<Refusals>) -> Self {
        self.refusals = Some(refusals);
        self
    }
    pub fn metrics(mut self, metrics: Arc<ProxyMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }
    pub fn self_gate(mut self, gate: Arc<dyn ProxySelfGate>) -> Self {
        self.self_gate = Some(gate);
        self
    }
    /// The shared max-dequeue-age recorder feeding the self-gate's ELU arm.
    /// Pass ONE clone to every recv-shard core so the window max spans the
    /// whole intake; defaults to a private (unread) recorder.
    pub fn intake_age(mut self, recorder: IntakeAgeRecorder) -> Self {
        self.intake_age = Some(recorder);
        self
    }
    pub fn cancel_lru(mut self, lru: Arc<CancelBranchLru>) -> Self {
        self.cancel_lru = Some(lru);
        self
    }
    pub fn resolver(mut self, resolver: Arc<dyn HostResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }
    /// Cache TTLs / caps for the named-target resolver (defaults:
    /// [`ResolverConfig::default`] — positive 60 s, negative 5 s). The runner
    /// env-exposes the TTLs as `PROXY_RESOLVER_POSITIVE_TTL_MS` /
    /// `PROXY_RESOLVER_NEGATIVE_TTL_MS`.
    pub fn resolver_config(mut self, cfg: ResolverConfig) -> Self {
        self.resolver_cfg = Some(cfg);
        self
    }
    /// Recv-shard index (the endpoint-stats slot). Defaults to 0; the sharded
    /// runner numbers its cores 0..N.
    pub fn shard(mut self, shard: usize) -> Self {
        self.shard = shard;
        self
    }
    /// The per-call trace registry (ADR-0026). Pass ONE clone to every recv
    /// shard so the active-trace cap bounds the process; the default is an
    /// environment-read registry, inert without an OTLP endpoint.
    pub fn traces(mut self, traces: Arc<ProxyTraces>) -> Self {
        self.traces = Some(traces);
        self
    }
    /// The liveness stamps ([`ShardPulse`]) — ONE across every recv shard,
    /// sized to the shard count, read by the runner's readiness gate. The
    /// default is a private one nobody reads.
    pub fn pulse(mut self, pulse: Arc<ShardPulse>) -> Self {
        self.pulse = Some(pulse);
        self
    }
    /// The proxy's first worker view, latched by its
    /// [`HealthProbe`](crate::health::HealthProbe) — ONE across every recv
    /// shard. Before it, a request with no target is dropped unanswered. The
    /// default is a view already given, for a core whose worker health its
    /// wiring sets.
    pub fn first_view(mut self, view: Arc<FirstView>) -> Self {
        self.first_view = Some(view);
        self
    }

    /// Finish into a [`ProxyCore`] bound on `endpoint`.
    pub fn build(self, endpoint: Box<dyn UdpEndpoint>) -> ProxyCore {
        let clock = self.clock.unwrap_or_else(Clock::system);
        let traces = self.traces.unwrap_or_else(|| Arc::new(ProxyTraces::from_env(clock.now_ms())));
        let refusals = self.refusals.unwrap_or_else(|| Arc::new(Refusals::new(&self.id_gen, 0)));
        ProxyCore::new(ProxyCoreParts {
            endpoint,
            advertised: self.advertised,
            external: self.external,
            strategy: self.strategy,
            registry: self.registry,
            cancel_lru: self
                .cancel_lru
                .unwrap_or_else(|| Arc::new(CancelBranchLru::with_clock(clock.clone()))),
            refusals,
            clock,
            metrics: self.metrics.unwrap_or_else(|| Arc::new(ProxyMetrics::new())),
            self_gate: self.self_gate.unwrap_or_else(|| Arc::new(AlwaysAdmitGate)),
            intake_age: self.intake_age.unwrap_or_default(),
            resolver: self.resolver.unwrap_or_else(|| Arc::new(SystemResolver)),
            resolver_cfg: self.resolver_cfg.unwrap_or_default(),
            shard: self.shard,
            traces,
            pulse: self.pulse.unwrap_or_else(|| Arc::new(ShardPulse::new(self.shard + 1))),
            first_view: self.first_view.unwrap_or_else(FirstView::given),
        })
    }
}
