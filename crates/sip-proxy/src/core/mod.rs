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
mod departed_originator_tests;
#[cfg(test)]
mod dual_face_tests;
mod face_pick;
#[cfg(test)]
mod first_view_tests;
#[cfg(test)]
mod liveness_routing_tests;
mod request;
mod response;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_net::UdpEndpoint;
use sip_txn::parse_guard::{parse_datagram, ParserPanicked};

use crate::addr::ProxyAddr;
use crate::cancel_lru::CancelBranchLru;
use crate::face::FaceCidrs;
use crate::health::FirstView;
use crate::liveness::ShardPulse;
use crate::observability::metrics::Face;
use crate::observability::{ProxyMetrics, UdpShardStats};
use crate::refusals::Refusals;
use crate::registry::WorkerRegistry;
use crate::resolver::{HostResolver, NamedForwarder, ResolverConfig};
use crate::self_gate::{IntakeAgeRecorder, ProxySelfGate};
use crate::strategy::RoutingStrategy;
use crate::trace::ProxyTraces;
pub use builder::ProxyCoreBuilder;
use face_pick::FacePickSendEndpoint;

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
/// UDP socket bound on the caller plane, its own advertise, and the internal
/// CIDR classifier that picks the egress face per destination IP. Handed to
/// [`ProxyCoreBuilder::external_face`]; when absent the core is the classic
/// single-face proxy.
pub struct ExternalFaceParts {
    /// The external-plane socket (bound on the external VIP).
    pub endpoint: Box<dyn UdpEndpoint>,
    /// The `host:port` stamped on Via / Record-Route for external-face egress.
    pub advertised: ProxyAddr,
    /// Destination IP ∈ any listed CIDR → internal face; else external.
    pub int_cidrs: FaceCidrs,
}

/// The live external face inside a running core.
struct ExternalFace {
    endpoint: Arc<dyn UdpEndpoint>,
    advertised: ProxyAddr,
    int_cidrs: FaceCidrs,
}

/// The dependency bundle for a [`ProxyCore`] (avoids a 10-argument
/// constructor). Crate-private: external wiring goes through
/// [`ProxyCoreBuilder`], which is the one place the defaults live.
pub(crate) struct ProxyCoreParts {
    pub endpoint: Box<dyn UdpEndpoint>,
    /// The address the proxy stamps on Via / Record-Route (its advertised
    /// `host:port`). Usually the endpoint's bound address.
    pub advertised: ProxyAddr,
    /// Dual-face mode: the external-plane socket + advertise + face picker.
    /// `None` → single-face.
    pub external: Option<ExternalFaceParts>,
    pub strategy: Arc<dyn RoutingStrategy>,
    pub registry: Arc<dyn WorkerRegistry>,
    pub cancel_lru: Arc<CancelBranchLru>,
    /// The answers to requests the proxy refuses itself, shared by every shard.
    pub refusals: Arc<Refusals>,
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
}

/// The stateless proxy.
pub struct ProxyCore {
    /// The internal-face socket (single-face mode: the ONLY socket).
    endpoint: Arc<dyn UdpEndpoint>,
    /// The internal-face advertise (single-face mode: THE advertise).
    advertised: ProxyAddr,
    /// Dual-face mode: the external-plane socket/advertise/picker.
    external: Option<ExternalFace>,
    strategy: Arc<dyn RoutingStrategy>,
    registry: Arc<dyn WorkerRegistry>,
    cancel_lru: Arc<CancelBranchLru>,
    refusals: Arc<Refusals>,
    clock: Clock,
    metrics: Arc<ProxyMetrics>,
    self_gate: Arc<dyn ProxySelfGate>,
    intake_age: IntakeAgeRecorder,
    parser: CustomParser,
    /// Off-loop send path for DNS-named targets (cache + single-flight resolve).
    named: NamedForwarder,
    shard: usize,
    pub(super) traces: Arc<ProxyTraces>,
    pulse: Arc<ShardPulse>,
    first_view: Arc<FirstView>,
}

impl ProxyCore {
    pub(crate) fn new(parts: ProxyCoreParts) -> Self {
        let endpoint: Arc<dyn UdpEndpoint> = Arc::from(parts.endpoint);
        let external = parts.external.map(|e| ExternalFace {
            endpoint: Arc::from(e.endpoint),
            advertised: e.advertised,
            int_cidrs: e.int_cidrs,
        });
        // The named-target (DNS) forwarder resolves off-path and only then
        // sends to the resolved SocketAddr; in dual-face mode hand it a
        // face-picking send seam so a resolved destination still egresses on
        // the face its IP lives on. Single-face keeps the raw endpoint.
        let named_ep: Arc<dyn UdpEndpoint> = match &external {
            Some(ext) => Arc::new(FacePickSendEndpoint {
                internal: endpoint.clone(),
                external: ext.endpoint.clone(),
                int_cidrs: ext.int_cidrs.clone(),
            }),
            None => endpoint.clone(),
        };
        let named = NamedForwarder::new(
            named_ep,
            parts.resolver,
            parts.resolver_cfg,
            parts.clock.clone(),
            parts.metrics.clone(),
        );
        Self {
            endpoint,
            advertised: parts.advertised,
            external,
            strategy: parts.strategy,
            registry: parts.registry,
            cancel_lru: parts.cancel_lru,
            refusals: parts.refusals,
            clock: parts.clock,
            metrics: parts.metrics,
            self_gate: parts.self_gate,
            intake_age: parts.intake_age,
            parser: CustomParser::default(),
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
    // Single-face mode (external = None): every helper below degenerates to
    // internal endpoint, internal advertise, `is_self_addr` == "matches the
    // one advertise".

    /// Is `ip` on the internal plane? Single-face: everything is internal.
    fn ip_is_internal(&self, ip: IpAddr) -> bool {
        match &self.external {
            Some(ext) => ext.int_cidrs.contains_ip(ip),
            None => true,
        }
    }

    /// The send socket for a concrete destination (the egress face picker).
    fn egress_endpoint(&self, dst: SocketAddr) -> &Arc<dyn UdpEndpoint> {
        match &self.external {
            Some(ext) if !ext.int_cidrs.contains_ip(dst.ip()) => &ext.endpoint,
            _ => &self.endpoint,
        }
    }

    /// The metrics label of the face a destination egresses on.
    fn egress_face(&self, dst: SocketAddr) -> Face {
        if self.ip_is_internal(dst.ip()) {
            Face::Internal
        } else {
            Face::External
        }
    }

    /// The advertise stamped for egress **toward `target`** (the Via sent-by of
    /// a forwarded request, the proxy hop inside a synthesized ACK). An
    /// IP-literal target classifies by CIDR; a DNS-named target (a worker-
    /// outbound b-leg R-URI — callers/callees live on the external plane, the
    /// worker pool is always IP literals) classifies **external** when a second
    /// face exists.
    pub(super) fn egress_advertised(&self, target: &ProxyAddr) -> &ProxyAddr {
        match &self.external {
            Some(ext) => match target.to_socket_addr() {
                Some(sa) if ext.int_cidrs.contains_ip(sa.ip()) => &self.advertised,
                _ => &ext.advertised,
            },
            None => &self.advertised,
        }
    }

    /// The advertise of the face `ip` lives on (the ingress-face advertise for
    /// a request arriving from that source).
    pub(super) fn advertised_for_ip(&self, ip: IpAddr) -> &ProxyAddr {
        match &self.external {
            Some(ext) if !ext.int_cidrs.contains_ip(ip) => &ext.advertised,
            _ => &self.advertised,
        }
    }

    /// Does `host:port` name THIS proxy — either face's advertise? The
    /// self-identity check for Route popping (§16.4), the response top-Via
    /// ownership check (§16.7.3), and Record-Route cookie recovery: in
    /// dual-face mode the proxy's Via/Record-Route entries legitimately carry
    /// EITHER face's advertise.
    pub(super) fn is_self_addr(&self, host: &str, port: u16) -> bool {
        if host == self.advertised.host && port == self.advertised.port {
            return true;
        }
        match &self.external {
            Some(ext) => host == ext.advertised.host && port == ext.advertised.port,
            None => false,
        }
    }

    /// Pin + pre-resolve DNS-named forward targets (`PROXY_RESOLVER_PREWARM`)
    /// so their FIRST b-leg forward is a warm cache hit; pinned names are kept
    /// permanently warm by the resolver's proactive refresh. Runs off the
    /// serving path and never blocks the caller (boot must not wait on DNS).
    /// See [`crate::resolver`].
    pub fn prewarm_named_targets(&self, targets: Vec<ProxyAddr>) {
        self.named.prewarm(targets);
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.now_ms()).unwrap_or(0)
    }

    /// [`now_ms`](Self::now_ms), signed as the per-call traces stamp it.
    fn trace_ms(&self) -> i64 {
        self.clock.now_ms().max(0)
    }

    /// Send raw bytes to a [`ProxyAddr`] target. An IP-literal host (the fast
    /// path: registry workers and the simulated fabric are always IP literals)
    /// is sent inline; a DNS name — e.g. a `sipp-uas` pod FQDN in a
    /// worker-outbound b-leg R-URI — goes through the [`NamedForwarder`], which
    /// serves cache hits inline and resolves misses on a spawned task so the
    /// recv loop NEVER waits on DNS (see [`crate::resolver`]).
    async fn send_to(&self, bytes: &[u8], target: &ProxyAddr) {
        if let Some(dst) = target.to_socket_addr() {
            self.metrics.record_face_egress(self.egress_face(dst));
            if let Err(e) = self.egress_endpoint(dst).send_to(bytes, dst).await {
                self.metrics.record_send_failure();
                // Per-peer attribution (sip_proxy_peer_failures_total{...,
                // kind=…}): classify against the registry (a known worker is
                // internal and always its own series, else external, under the cap), and name an
                // oversize message as such rather than blaming the peer.
                self.metrics.record_peer_failure(
                    &dst,
                    self.classify_peer(target),
                    peer_failure_kind(&e),
                );
            }
            return;
        }
        self.named.send(bytes, target).await;
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

    /// Reply to the packet's source (egresses on the face the source lives on
    /// — which is the face the packet arrived on).
    async fn reply_to_source(&self, bytes: &[u8], src: SocketAddr) {
        self.metrics.record_face_egress(self.egress_face(src));
        if let Err(e) = self.egress_endpoint(src).send_to(bytes, src).await {
            self.metrics.record_send_failure();
            // The reply source is whoever sent us the packet (typically an
            // upstream UAC/UAS — external); still classify via the registry in
            // case it is a worker.
            let target = ProxyAddr::from(src);
            self.metrics.record_peer_failure(
                &src,
                self.classify_peer(&target),
                peer_failure_kind(&e),
            );
        }
    }

    /// The recv loop. Runs until the endpoint's queue is closed (the endpoint
    /// is dropped). Parse failures are dropped silently (RFC 3261 §16.3); a
    /// datagram the parser panics on is dropped and counted, and the loop
    /// goes on.
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
            let src = pkt.src;
            // Hand the receive buffer to the parser instead of lending it: the
            // message's `raw`/`body` then share it and no packet byte is copied.
            let raw = bytes::Bytes::from(pkt.raw);
            let msg = match parse_datagram(raw, |raw| self.parser.parse_shared(raw)) {
                Ok(Ok(msg)) => msg,
                // Malformed datagram — drop silently.
                Ok(Err(_)) => {
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
