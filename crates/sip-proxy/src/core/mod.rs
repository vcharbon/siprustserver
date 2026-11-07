//! [`ProxyCore`] — the stateless proxy data path (single-endpoint K8s-LB
//! mode; a SIP registrar does NOT live here).
//!
//! It binds one UDP endpoint, runs a recv loop, parses each datagram, and
//! dispatches to [`request`](self) / [`response`](self) handling. It owns the
//! routing-policy seam ([`RoutingStrategy`]), the worker registry, the
//! `(Call-ID|CSeq#)` LRU, the [`Refusals`] that answer the requests it refuses
//! itself, a [`Clock`], metrics, a logger, and the self-gate (intake-age/CPS
//! admission). A hop thaws the received
//! message into a draft, touches only the routing headers, and freezes it to
//! the wire bytes it forwards — every untouched line is memcpy'd, never
//! re-parsed (ADR-0025). Its defaults live in [`ProxyCoreBuilder`].

mod builder;
#[cfg(test)]
mod crossing_tests;
#[cfg(test)]
mod departed_originator_tests;
#[cfg(test)]
mod dual_face_tests;
#[cfg(test)]
mod first_view_tests;
#[cfg(test)]
mod liveness_routing_tests;
#[cfg(test)]
mod maddr_tests;
#[cfg(test)]
mod owner_rule_tests;
mod request;
mod response;
#[cfg(test)]
mod worker_ack_tests;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_message::ip_literal::same_host;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::{parse_datagram, ParserPanicked, UdpEndpoint, UdpPacket};

use crate::addr::ProxyAddr;
use crate::cancel_lru::CancelBranchLru;
use crate::face::EgressPolicy;
use crate::health::FirstView;
use crate::liveness::ShardPulse;
use crate::observability::metrics::Face;
use crate::observability::{ProxyMetrics, UdpShardStats};
use crate::parse_refusals::ParseRefusals;
use crate::refusals::Refusals;
use crate::registry::WorkerRegistry;
use crate::resolver::{HostResolver, NamedForwarder, ResolverConfig};
use crate::self_gate::{IntakeAgeRecorder, ProxySelfGate};
use crate::strategy::RoutingStrategy;
use crate::trace::ProxyTraces;
pub use builder::ProxyCoreBuilder;

/// The per-peer bucket a failed send belongs in: an oversize datagram accuses
/// the message, everything else the peer or the socket.
fn peer_failure_kind(
    err: &sip_net::SendError,
) -> crate::observability::peer_failures::PeerFailureKind {
    use crate::observability::peer_failures::PeerFailureKind;
    match err.kind {
        sip_net::SendErrorKind::MessageTooLong => PeerFailureKind::MessageTooLong,
        _ => PeerFailureKind::SendFailure,
    }
}

/// Methods that create a dialog (RFC 3261) — the proxy inserts a Record-Route
/// only for these.
fn is_dialog_creating(method: &str) -> bool {
    method == "INVITE" || method == "SUBSCRIBE"
}

/// The **external face** of a dual-face (multi-homed edge) proxy — the second
/// UDP socket bound on the caller plane and its own advertise. Handed to
/// [`ProxyCoreBuilder::external_face`]; when absent the core is the classic
/// single-face proxy.
pub struct ExternalFaceParts {
    /// The external-plane socket (bound on the external VIP).
    pub endpoint: Box<dyn UdpEndpoint>,
    /// The `host:port` stamped on Via / Record-Route for external-face egress.
    pub advertised: ProxyAddr,
}

/// The live external face inside a running core.
struct ExternalFace {
    endpoint: Arc<dyn UdpEndpoint>,
    advertised: ProxyAddr,
    /// Off-loop send path for DNS-named targets leaving on this face.
    named: NamedForwarder,
}

/// The dependency bundle for a [`ProxyCore`] (avoids a 10-argument
/// constructor). Crate-private: external wiring goes through
/// [`ProxyCoreBuilder`], which is the one place the defaults live.
pub(crate) struct ProxyCoreParts {
    pub endpoint: Box<dyn UdpEndpoint>,
    /// The address the proxy stamps on Via / Record-Route (its advertised
    /// `host:port`). Usually the endpoint's bound address.
    pub advertised: ProxyAddr,
    /// Dual-face mode: the external-plane socket + advertise. `None` →
    /// single-face.
    pub external: Option<ExternalFaceParts>,
    pub strategy: Arc<dyn RoutingStrategy>,
    pub registry: Arc<dyn WorkerRegistry>,
    pub cancel_lru: Arc<CancelBranchLru>,
    /// The answers to requests the proxy refuses itself, shared by every shard.
    pub refusals: Arc<Refusals>,
    /// The answers to datagrams the parser refused, shared by every shard.
    pub parse_refusals: Arc<ParseRefusals>,
    pub clock: Clock,
    pub metrics: Arc<ProxyMetrics>,
    pub self_gate: Arc<dyn ProxySelfGate>,
    /// Shared max-dequeue-age cell feeding the self-gate's ELU arm; the recv
    /// loop records every dequeued packet's age into it (one recorder across
    /// all shards and both faces).
    pub intake_age: IntakeAgeRecorder,
    /// Resolver for DNS-named forward targets (worker-outbound b-leg R-URIs).
    /// IP-literal traffic never touches it; see [`crate::resolver`].
    pub resolver: Arc<dyn HostResolver>,
    /// Cache TTLs / caps for the named-target resolver.
    pub resolver_cfg: ResolverConfig,
    /// Recv-shard index for this core's endpoint-stats slot (`0` for the
    /// single-socket wiring; `0..N` when the runner shards the recv path over
    /// N reuse-port sockets).
    pub shard: usize,
    /// The per-call trace tier (ADR-0026). ONE registry across every recv
    /// shard, so the active-trace cap bounds the process rather than a socket.
    pub traces: Arc<ProxyTraces>,
    /// Liveness stamps, one cell per shard; this core owns cell `shard`.
    pub pulse: Arc<ShardPulse>,
    /// The proxy's first worker view, shared with its probe.
    pub first_view: Arc<FirstView>,
    /// What reads a datagram no intake hook parsed: the process's one parser.
    pub parser: CustomParser,
}

/// The stateless proxy.
pub struct ProxyCore {
    /// The internal-face socket (single-face mode: the ONLY socket).
    endpoint: Arc<dyn UdpEndpoint>,
    /// The internal-face advertise (single-face mode: THE advertise).
    advertised: ProxyAddr,
    /// Dual-face mode: the external-plane socket/advertise.
    external: Option<ExternalFace>,
    /// Which face a message leaves on.
    egress: EgressPolicy,
    strategy: Arc<dyn RoutingStrategy>,
    registry: Arc<dyn WorkerRegistry>,
    cancel_lru: Arc<CancelBranchLru>,
    refusals: Arc<Refusals>,
    parse_refusals: Arc<ParseRefusals>,
    clock: Clock,
    metrics: Arc<ProxyMetrics>,
    self_gate: Arc<dyn ProxySelfGate>,
    intake_age: IntakeAgeRecorder,
    parser: CustomParser,
    /// Off-loop send path for DNS-named targets leaving on the internal face
    /// (cache + single-flight resolve).
    named: NamedForwarder,
    shard: usize,
    pub(super) traces: Arc<ProxyTraces>,
    pulse: Arc<ShardPulse>,
    first_view: Arc<FirstView>,
}

impl ProxyCore {
    pub(crate) fn new(parts: ProxyCoreParts) -> Self {
        let endpoint: Arc<dyn UdpEndpoint> = Arc::from(parts.endpoint);
        let forwarder = |endpoint: &Arc<dyn UdpEndpoint>| {
            NamedForwarder::new(
                endpoint.clone(),
                parts.resolver.clone(),
                parts.resolver_cfg,
                parts.clock.clone(),
                parts.metrics.clone(),
            )
        };
        let named = forwarder(&endpoint);
        let external = parts.external.map(|e| {
            let endpoint: Arc<dyn UdpEndpoint> = Arc::from(e.endpoint);
            let named = forwarder(&endpoint);
            ExternalFace { endpoint, advertised: e.advertised, named }
        });
        let egress =
            if external.is_some() { EgressPolicy::Crossing } else { EgressPolicy::SingleFace };
        Self {
            endpoint,
            advertised: parts.advertised,
            external,
            egress,
            strategy: parts.strategy,
            registry: parts.registry,
            cancel_lru: parts.cancel_lru,
            refusals: parts.refusals,
            parse_refusals: parts.parse_refusals,
            clock: parts.clock,
            metrics: parts.metrics,
            self_gate: parts.self_gate,
            intake_age: parts.intake_age,
            parser: parts.parser,
            named,
            shard: parts.shard,
            pulse: parts.pulse,
            traces: parts.traces,
            first_view: parts.first_view,
        }
    }

    /// This proxy's per-call trace registry (ADR-0026) — the seam a runner
    /// shares across recv shards and a test drives its own gate through.
    pub fn traces(&self) -> Arc<ProxyTraces> {
        self.traces.clone()
    }

    /// A handle to this proxy's metrics (for SUT assertions / a metrics server).
    pub fn metrics(&self) -> Arc<ProxyMetrics> {
        self.metrics.clone()
    }

    pub fn advertised(&self) -> &ProxyAddr {
        &self.advertised
    }

    // ── Dual-face plumbing ──────────────────────────────────────────────────
    // A single-face proxy has one socket and one advertise, the internal ones,
    // and every message it handles is on that face.

    /// The socket of `face`.
    fn endpoint_of(&self, face: Face) -> &Arc<dyn UdpEndpoint> {
        match (&self.external, face) {
            (Some(ext), Face::External) => &ext.endpoint,
            _ => &self.endpoint,
        }
    }

    /// The advertise of `face`: what a Via or Record-Route entry states for a
    /// message leaving on it, or for later requests to reach the proxy there.
    pub(super) fn advertised_of(&self, face: Face) -> &ProxyAddr {
        match (&self.external, face) {
            (Some(ext), Face::External) => &ext.advertised,
            _ => &self.advertised,
        }
    }

    /// The named-target send path of `face`.
    fn named_of(&self, face: Face) -> &NamedForwarder {
        match (&self.external, face) {
            (Some(ext), Face::External) => &ext.named,
            _ => &self.named,
        }
    }

    /// Does `host:port` name THIS proxy — either face's advertise? The
    /// self-identity check for Route popping (§16.4), the response top-Via
    /// ownership check (§16.7.3), and Record-Route cookie recovery: in
    /// dual-face mode the proxy's Via/Record-Route entries legitimately carry
    /// EITHER face's advertise. Hosts compare as what they name
    /// ([`same_host`]): another spelling of this proxy's address is this proxy.
    pub(super) fn is_self_addr(&self, host: &str, port: u16) -> bool {
        let names = |at: &ProxyAddr| port == at.port && same_host(host, &at.host);
        names(&self.advertised) || self.external.as_ref().is_some_and(|e| names(&e.advertised))
    }

    /// Whether `src` is one of this proxy's own addresses: a face's bind or its
    /// IP-literal advertise. A request from there was forwarded by this proxy
    /// and came back to it.
    pub(super) fn is_own_source(&self, src: SocketAddr) -> bool {
        let names = |endpoint: &Arc<dyn UdpEndpoint>, at: &ProxyAddr| {
            endpoint.local_addr() == src || at.to_socket_addr() == Some(src)
        };
        names(&self.endpoint, &self.advertised)
            || self.external.as_ref().is_some_and(|e| names(&e.endpoint, &e.advertised))
    }

    /// Pin + pre-resolve DNS-named forward targets (`PROXY_RESOLVER_PREWARM`)
    /// so their FIRST b-leg forward is a warm cache hit; pinned names are kept
    /// permanently warm by the resolver's proactive refresh. Runs off the
    /// serving path and never blocks the caller (boot must not wait on DNS).
    /// See [`crate::resolver`]. A name is a worker's dialled target, so it
    /// warms the face that request leaves on.
    pub fn prewarm_named_targets(&self, targets: Vec<ProxyAddr>) {
        self.external.as_ref().map_or(&self.named, |ext| &ext.named).prewarm(targets);
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.now_ms()).unwrap_or(0)
    }

    /// [`now_ms`](Self::now_ms), signed as the per-call traces stamp it.
    fn trace_ms(&self) -> i64 {
        self.clock.now_ms().max(0)
    }

    /// Send raw bytes to a [`ProxyAddr`] target, on `face`. An IP-literal host
    /// (the fast path: registry workers and the simulated fabric are always IP
    /// literals) is sent inline; a DNS name — e.g. a `sipp-uas` pod FQDN in a
    /// worker-outbound b-leg R-URI — goes through the [`NamedForwarder`], which
    /// serves cache hits inline and resolves misses on a spawned task so the
    /// recv loop NEVER waits on DNS (see [`crate::resolver`]). Every refusal is
    /// counted; the kind of one the socket answered inline is returned, `None`
    /// when the datagram left or a resolve sends it later.
    async fn send_to(
        &self,
        bytes: &[u8],
        target: &ProxyAddr,
        face: Face,
    ) -> Option<sip_net::SendErrorKind> {
        self.metrics.record_face_egress(face);
        let Some(dst) = target.to_socket_addr() else {
            return self.named_of(face).send(bytes, target).await;
        };
        let e = self.endpoint_of(face).send_to(bytes, dst).await.err()?;
        self.metrics.record_send_failure();
        // Per-peer attribution (sip_proxy_peer_failures_total{..., kind=…}):
        // classify against the registry (a known worker is internal and always
        // its own series, else external, under the cap), and name an oversize
        // message as such rather than blaming the peer.
        self.metrics.record_peer_failure(&dst, self.classify_peer(target), peer_failure_kind(&e));
        Some(e.kind)
    }

    /// Classify a forward target internal/external for the per-peer metric: a
    /// destination that resolves to a known worker (registry) is the in-cluster
    /// data path and is pinned; everything else (a UAC/UAS, a DNS-named callee)
    /// is external and under the family's cap.
    fn classify_peer(&self, target: &ProxyAddr) -> crate::observability::peer_failures::PeerScope {
        if self.registry.lookup_by_address(target).is_some() {
            crate::observability::peer_failures::PeerScope::Internal
        } else {
            crate::observability::peer_failures::PeerScope::External
        }
    }

    /// Send an own final to `dst`, on `face`, the one its request arrived on.
    async fn send_response(&self, bytes: &[u8], dst: SocketAddr, face: Face) {
        self.metrics.record_face_egress(face);
        if let Err(e) = self.endpoint_of(face).send_to(bytes, dst).await {
            self.metrics.record_send_failure();
            // The destination is whoever the request's Via names (typically an
            // upstream UAC/UAS — external); still classify via the registry in
            // case it is a worker.
            let target = ProxyAddr::from(dst);
            self.metrics.record_peer_failure(
                &dst,
                self.classify_peer(&target),
                peer_failure_kind(&e),
            );
        }
    }

    /// The recv loop. Runs until the endpoint's queue is closed (the endpoint
    /// is dropped). A datagram is parsed once: the loop takes the parse its
    /// receive path made and parses only a datagram that came without one. A
    /// refused one is answered or dropped (ADR-0054); one the parser panics
    /// on is dropped and counted, and the loop goes on.
    pub async fn run(self) {
        // The shared `(Call-ID|CSeq#)` LRU sweeper — spawned exactly once per
        // LRU however many recv-shard cores share it (see
        // [`CancelBranchLru::ensure_sweeper`]).
        self.cancel_lru.ensure_sweeper(self.metrics.clone());

        // Per-shard endpoint-stats publisher: a tail-dropping queue otherwise
        // shows 100% forwarded on dashboards. Each core owns its endpoint's
        // slot, so N shards never stomp one gauge; the metrics render the
        // cross-shard aggregate. The AbortOnDrop guard aborts it even when the
        // whole recv-loop TASK is aborted (a supervisor/harness abort drops
        // this future mid-`recv`): without it the stats task would keep a strong
        // endpoint `Arc` alive forever, so the socket (and on the simulated
        // fabric the bind address itself) would leak past the core's death and
        // block rebinding the same VIP for a takeover proxy.
        let stats = {
            let metrics = self.metrics.clone();
            let endpoint = self.endpoint.clone();
            let ext_endpoint = self.external.as_ref().map(|e| e.endpoint.clone());
            let shard = self.shard;
            crate::supervise::spawn(crate::supervise::SHARD_STATS, move || {
                let (metrics, endpoint) = (metrics.clone(), endpoint.clone());
                let ext_endpoint = ext_endpoint.clone();
                async move {
                    #[expect(clippy::disallowed_methods, reason = "the sweep interval is not zero")]
                    let mut tick = tokio::time::interval(Duration::from_millis(
                        crate::cancel_lru::DEFAULT_SWEEP_INTERVAL_MS,
                    ));
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    loop {
                        tick.tick().await;
                        metrics.set_udp_endpoint_stats(
                            shard,
                            UdpShardStats::of_faces(&*endpoint, ext_endpoint.as_deref()),
                        );
                    }
                }
            })
        };
        let _stats_guard = AbortOnDrop(stats);

        loop {
            // Dual-face: serve BOTH sockets from the one core (one process,
            // one state shared across faces). Either socket closing ends the
            // core: the faces are one identity and live and die together (the
            // runner supervises the core task and exits for restart). The face
            // a datagram arrived on is its trust (ADR-0048).
            let (pkt, face) = match &self.external {
                Some(ext) => tokio::select! {
                    p = self.endpoint.recv() => (p, Face::Internal),
                    p = ext.endpoint.recv() => (p, Face::External),
                },
                None => (self.endpoint.recv().await, Face::Internal),
            };
            let Some(pkt) = pkt else { break };
            let now_ms = self.now_ms();
            self.pulse.took_packet(self.shard, now_ms);
            // Intake-saturation signal: the packet's age at dequeue (arrival →
            // now) feeds the self-gate's ELU arm through the shared recorder.
            // Both readings sit on THIS core's `Clock` — the endpoint stamps
            // arrivals on it (`BindUdpOpts::clock`) — so the age is a duration
            // on one timeline and carries no wall-vs-monotonic drift.
            self.intake_age.record(now_ms.saturating_sub(pkt.arrival_ms));
            self.metrics.record_face_ingress(face);
            let UdpPacket { raw, src, parsed, .. } = pkt;
            // The receive path's parse, else one made here; either way the
            // message's `raw`/`body` share the receive buffer.
            let parsed = match parsed {
                Some(parsed) => *parsed,
                None => parse_datagram(raw.clone(), |b| self.parser.parse_shared(b)),
            };
            let msg = match parsed {
                Ok(Ok(msg)) => msg,
                Ok(Err(error)) => {
                    crate::refused_sample::sample(src, &error, &raw, now_ms);
                    self.refuse_unparsed(&error, src, face).await;
                    self.pulse.waiting(self.shard);
                    continue;
                }
                Err(ParserPanicked) => {
                    self.metrics.record_parse_panic();
                    self.pulse.waiting(self.shard);
                    continue;
                }
            };
            match msg {
                SipMessage::Request(_) => self.handle_request(msg, src, face).await,
                SipMessage::Response(resp) => self.handle_response(resp, src, face).await,
            }
            self.pulse.waiting(self.shard);
        }
    }
}

/// Abort a spawned task when dropped — ties a helper task's lifetime to the
/// owning future (surviving task aborts, not just clean loop exits).
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod sweeper_tests;
