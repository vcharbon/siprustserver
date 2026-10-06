//! [`ProxyMetrics`] — atomics-backed counters/gauges for the proxy data path
//! (port of `observability/Metrics.ts`). The source uses Effect `Metric`s; here
//! we keep live atomics + small labeled maps and render Prometheus text
//! ([`ProxyMetrics::prometheus_text`]) for the `/metrics` endpoint.
//!
//! Mirrors the source metric names so dashboards transfer: `sip_messages_total`,
//! `sip_routing_decision_total`, `sip_routing_duration_seconds` (histogram),
//! `sip_proxy_hmac_failures_total`, `sip_worker_health`, etc.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use metric_catalogue::{FixedCounts, HistogramValue, OpenRows};
use sip_message::method::Method;
use sip_net::UdpEndpoint;

use super::catalogue;
use crate::resolver::{outcome, refresh_outcome};
use crate::strategy::DecodeResult;

/// Inbound vs outbound: the first half of `sip_messages_total`'s
/// `label="direction:result"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Inbound,
    Outbound,
}

impl Direction {
    /// Every direction, in declaration order.
    pub const ALL: [Direction; 2] = [Direction::Inbound, Direction::Outbound];
}

/// Which proxy face a datagram crossed, for
/// `sip_proxy_face_messages_total{label="face:direction"}` (dual-face mode). A
/// single-face proxy records everything as `int`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Face {
    Internal,
    External,
}

impl Face {
    /// Every face, in declaration order.
    pub const ALL: [Face; 2] = [Face::Internal, Face::External];

    /// The face's metrics label, and the value a trace event names it by.
    pub const fn as_str(self) -> &'static str {
        match self {
            Face::Internal => "int",
            Face::External => "ext",
        }
    }
}

/// How a message was handled: the second half of `sip_messages_total`'s
/// `label="direction:result"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageResult {
    Forwarded,
    Responded,
    Dropped,
}

impl MessageResult {
    /// Every result, in declaration order.
    pub const ALL: [MessageResult; 3] =
        [MessageResult::Forwarded, MessageResult::Responded, MessageResult::Dropped];
}

/// The routing decision taken, for `sip_routing_decision_total{kind}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingDecisionKind {
    SelectNew,
    DecodeForward,
    DecodeForwardBackup,
    LooseRoute,
    WorkerOutbound,
    Cancel,
    AckHop,
    Reject,
}

impl RoutingDecisionKind {
    /// Every kind, in declaration order.
    pub const ALL: [RoutingDecisionKind; 8] = [
        RoutingDecisionKind::SelectNew,
        RoutingDecisionKind::DecodeForward,
        RoutingDecisionKind::DecodeForwardBackup,
        RoutingDecisionKind::LooseRoute,
        RoutingDecisionKind::WorkerOutbound,
        RoutingDecisionKind::Cancel,
        RoutingDecisionKind::AckHop,
        RoutingDecisionKind::Reject,
    ];

    /// The decision's metrics label, and the value a trace event names it by.
    pub const fn as_str(self) -> &'static str {
        match self {
            RoutingDecisionKind::SelectNew => "select_new",
            RoutingDecisionKind::DecodeForward => "decode_forward",
            RoutingDecisionKind::DecodeForwardBackup => "decode_forward_backup",
            RoutingDecisionKind::LooseRoute => "loose_route",
            RoutingDecisionKind::WorkerOutbound => "worker_outbound",
            RoutingDecisionKind::Cancel => "cancel",
            RoutingDecisionKind::AckHop => "ack_hop",
            RoutingDecisionKind::Reject => "reject",
        }
    }
}

/// Why an HMAC verify failed, for `sip_proxy_hmac_failures_total{reason}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HmacFailureReason {
    Missing,
    Decode,
    Mismatch,
}

impl HmacFailureReason {
    /// Every reason, in declaration order.
    pub const ALL: [HmacFailureReason; 3] =
        [HmacFailureReason::Missing, HmacFailureReason::Decode, HmacFailureReason::Mismatch];

    /// The `reason` label.
    pub const fn as_str(self) -> &'static str {
        match self {
            HmacFailureReason::Missing => "missing",
            HmacFailureReason::Decode => "decode",
            HmacFailureReason::Mismatch => "mismatch",
        }
    }
}

/// How a CANCEL's lookup of its INVITE's remembered hop ended, for
/// `sip_proxy_cancel_lookups_total{outcome}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelLookup {
    /// The INVITE's hop was remembered: the CANCEL follows it.
    Hit,
    /// No hop remembered: the CANCEL is routed on its own.
    Miss,
}

impl CancelLookup {
    /// Every outcome, in declaration order.
    pub const ALL: [CancelLookup; 2] = [CancelLookup::Hit, CancelLookup::Miss];

    /// The `outcome` label.
    pub const fn as_str(self) -> &'static str {
        match self {
            CancelLookup::Hit => "hit",
            CancelLookup::Miss => "miss",
        }
    }
}

/// The fixed counter slot of a native method (uppercased by the caller);
/// `None` for an extension method, counted under its own label in
/// [`ProxyMetrics`]'s open rows, under the family's cap.
fn method_slot(method: &str) -> Option<usize> {
    Method::NATIVE_TOKENS.iter().position(|m| *m == method)
}

/// Count one `label` of the closed list `labels` on its slot of `counters`;
/// a label outside the list is a caller's bug.
fn count_outcome(counters: &[AtomicU64], labels: &[&str], label: &str) {
    match labels.iter().position(|l| *l == label) {
        Some(i) => {
            counters[i].fetch_add(1, Ordering::Relaxed);
        }
        None => debug_assert!(false, "{label} is no outcome of {labels:?}"),
    }
}

/// The five worker-health gauges (one set per registry: the count of workers in
/// each health state).
#[derive(Default)]
struct HealthGauges {
    alive: AtomicU64,
    draining: AtomicU64,
    not_ready: AtomicU64,
    unknown: AtomicU64,
    dead: AtomicU64,
}

/// One recv shard's endpoint state: receive-queue gauges and the endpoints'
/// lifetime counters, every field summed over the shard's faces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UdpShardStats {
    pub queue_depth: u64,
    pub queue_max: u64,
    pub enqueued: u64,
    pub tail_dropped: u64,
    pub intake_shed: u64,
    pub send_would_block: u64,
    /// Datagrams the kernel dropped on the shard's sockets before the recv
    /// pump read them (a full `SO_RCVBUF`).
    pub kernel_rx_dropped: u64,
}

impl UdpShardStats {
    /// The shard's stats over its internal face and, in dual-face mode, its
    /// external face, where callers arrive.
    pub fn of_faces(internal: &dyn UdpEndpoint, external: Option<&dyn UdpEndpoint>) -> Self {
        let mut stats = Self::of_endpoint(internal);
        if let Some(ext) = external {
            stats += Self::of_endpoint(ext);
        }
        stats
    }

    fn of_endpoint(ep: &dyn UdpEndpoint) -> Self {
        let c = ep.counters();
        Self {
            queue_depth: ep.queue_depth() as u64,
            queue_max: ep.queue_max() as u64,
            enqueued: c.enqueued,
            tail_dropped: c.tail_dropped,
            intake_shed: c.pre_ingress_dropped,
            send_would_block: c.send_would_block,
            kernel_rx_dropped: c.kernel_rx_dropped,
        }
    }
}

impl std::ops::AddAssign for UdpShardStats {
    fn add_assign(&mut self, o: Self) {
        self.queue_depth += o.queue_depth;
        self.queue_max += o.queue_max;
        self.enqueued += o.enqueued;
        self.tail_dropped += o.tail_dropped;
        self.intake_shed += o.intake_shed;
        self.send_would_block += o.send_would_block;
        self.kernel_rx_dropped += o.kernel_rx_dropped;
    }
}

/// Live proxy metrics. Cheap to share behind an `Arc`.
pub struct ProxyMetrics {
    /// `[direction][result]` — fixed slots, lock-free (multiple increments per
    /// packet on the hot path).
    messages: [[AtomicU64; 3]; 2],
    /// One slot per native method — lock-free; an extension method's count
    /// sits in `request_extensions`.
    requests: [AtomicU64; Method::NATIVE_TOKENS.len()],
    /// Extension methods, each under its own label, under the family's cap
    /// (the method token is wire-controlled).
    request_extensions: OpenRows,
    /// By CSeq method and status, under the family's cap (both are
    /// wire-controlled; the parser bounds the status to 100..=699).
    responses: OpenRows,
    calls: AtomicU64, // initial (dialog-creating, no To-tag) INVITEs
    routing_decisions: [AtomicU64; RoutingDecisionKind::ALL.len()], // indexed by RoutingDecisionKind
    hmac_failures: [AtomicU64; HmacFailureReason::ALL.len()],
    /// Locally decided rejections (`RouteOutcome` kind `reject`), keyed by a
    /// bounded static reason — where the reject also answers on the wire, the
    /// same string the SIP `Reason` header states. The per-cause split of the
    /// aggregate `sip_routing_decision_total{kind="reject"}`, so a
    /// proxy-generated 503 is attributable from metrics alone.
    rejects: FixedCounts,
    cancel_lookups: FixedCounts,
    decode_forward_promotions: FixedCounts,
    /// Cookie-routed requests forwarded to a primary inside its fresh-pod
    /// guard window because the cookie names no usable backup.
    fresh_pod_forwards: AtomicU64,
    overload_rejections: FixedCounts,
    /// Coarse (registry-aggregate) count of `WorkerLoadObserver::sweep_stale`
    /// floor events — an Alive worker whose OPTIONS replies stopped carrying a
    /// fresh `X-Overload` payload within `payload_stale_ms`, so AIMD halved its
    /// cap. Non-zero while ELU is healthy means the HealthProbe cycle exceeds the
    /// stale threshold (config invariant). ProxyMetrics is registry-aggregate, so
    /// this is the coarse stand-in for a per-`worker_id` counter that keeps a
    /// silently-floored cap diagnosable.
    overload_stale_decrease: AtomicU64,
    /// New-dialog INVITEs that BYPASSED the LB's overload gates because they are
    /// emergency: they skip the `above_critical` critical-filter (kept as
    /// candidates even when every alive worker is shedding) AND skip the
    /// per-worker AIMD token bucket (`try_consume_for`). The visibility series for
    /// emergency traffic skipping the load-balancer's overload path — non-zero
    /// under flood means emergency calls are correctly routed while non-emergency
    /// load is rate-capped. Counted at the LB select site (load_balancer.rs).
    lb_emergency_bypassed: AtomicU64,
    routing_duration_count: AtomicU64,
    routing_duration_sum_us: AtomicU64,
    record_route_inserted: AtomicU64,
    pending_invite_lru_size: AtomicU64,
    /// By outcome, indexed like `crate::resolver::outcome::ALL`.
    named_sends: [AtomicU64; outcome::ALL.len()],
    /// Proactive resolver refresh + startup prewarm events, keyed outcome
    /// (crate::resolver::refresh_outcome — closed set:
    /// refreshed|failed|idle_stopped|prewarmed|prewarm_failed). The
    /// cold-cache-kill visibility: `refreshed` moving means
    /// active names never expire cold; `prewarm_failed`/`failed` climbing
    /// means DNS is unhealthy while the old entries keep serving.
    resolver_refresh: [AtomicU64; refresh_outcome::ALL.len()],
    resolver_cache_size: AtomicU64,
    /// Outbound datagrams the endpoint failed to send (EPERM/ENOBUFS/...).
    /// `sip_messages_total{outbound,forwarded}` counts hand-off to the send
    /// path, so this is the delta dashboards need under overload.
    send_failures: AtomicU64,
    /// Endpoint receive-queue stats, one slot per recv shard, published by
    /// each core's maintenance tick — without them a tail-dropping queue shows
    /// 100% forwarded. Keyed by shard index so N reuse-port cores don't stomp
    /// one gauge; rendered as the cross-shard aggregate (sum). Not hot:
    /// written every sweep tick, read at render.
    udp_shards: Mutex<BTreeMap<usize, UdpShardStats>>,
    health: HealthGauges,
    /// `1` ⇒ the worker pool has **zero routable (`Alive`) workers** — the proxy
    /// can serve no new dialog. Set from the registry by the runner's health
    /// sampler (ADR-0012 D4): an empty/RBAC-forbidden EndpointSlice informer pool
    /// is otherwise silent (the proxy just black-holes every INVITE). Pairs with
    /// the `/readyz` gate; alert on `sip_proxy_worker_pool_empty == 1`.
    worker_pool_empty: AtomicU64,
    /// Recv shards currently past the stall threshold (see `liveness`).
    recv_shards_stalled: AtomicU64,
    /// Per-face traffic counters (dual-face mode): `[face][direction]` where
    /// face ∈ {int, ext} and direction ∈ {inbound (recv), outbound (egress)}.
    /// A single-face proxy only ever touches the `int` slots — cheap fixed
    /// atomics, no restructuring of the existing counters.
    face_messages: [[AtomicU64; 2]; 2],
    /// Per-peer failure/timeout counters
    /// (`sip_proxy_peer_failures_total{peer,scope,kind}`). Internal = resolves to
    /// a known worker (registry), always its own series; external = under the
    /// family's cap. See [`crate::observability::peer_failures::PeerFailures`].
    per_peer: crate::observability::peer_failures::PeerFailures,
}

impl Default for ProxyMetrics {
    fn default() -> Self {
        Self {
            messages: Default::default(),
            requests: Default::default(),
            request_extensions: OpenRows::new(&catalogue::REQUESTS),
            responses: OpenRows::new(&catalogue::RESPONSES),
            calls: Default::default(),
            routing_decisions: Default::default(),
            hmac_failures: Default::default(),
            rejects: FixedCounts::new(&catalogue::REJECTS),
            cancel_lookups: FixedCounts::new(&catalogue::CANCEL_LOOKUPS),
            decode_forward_promotions: FixedCounts::new(&catalogue::DECODE_FORWARD_PROMOTIONS),
            fresh_pod_forwards: Default::default(),
            overload_rejections: FixedCounts::new(&catalogue::OVERLOAD_REJECTIONS),
            overload_stale_decrease: Default::default(),
            lb_emergency_bypassed: Default::default(),
            routing_duration_count: Default::default(),
            routing_duration_sum_us: Default::default(),
            record_route_inserted: Default::default(),
            pending_invite_lru_size: Default::default(),
            named_sends: Default::default(),
            resolver_refresh: Default::default(),
            resolver_cache_size: Default::default(),
            send_failures: Default::default(),
            udp_shards: Default::default(),
            health: Default::default(),
            worker_pool_empty: Default::default(),
            recv_shards_stalled: Default::default(),
            face_messages: Default::default(),
            per_peer: Default::default(),
        }
    }
}

impl ProxyMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_message(&self, direction: Direction, result: MessageResult) {
        self.messages[direction as usize][result as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// Count one inbound request by SIP method (uppercased by the caller), for
    /// `sip_proxy_requests_total{method}`: a native method on its lock-free
    /// slot, an extension method under its own label, under the cap.
    pub fn record_request(&self, method: &str) {
        match method_slot(method) {
            Some(slot) => {
                self.requests[slot].fetch_add(1, Ordering::Relaxed);
            }
            None => self.request_extensions.add(&[method], 1),
        }
    }

    /// Count one inbound response by its CSeq method + status code, for
    /// `sip_proxy_responses_total{method,code}`, under the cap.
    pub fn record_response(&self, method: &str, code: u16) {
        self.responses.add(&[method, &code.to_string()], 1);
    }

    /// Count one new call: a dialog-creating INVITE with no To-tag (an initial
    /// out-of-dialog INVITE), for `sip_proxy_calls_total`.
    pub fn record_call(&self) {
        self.calls.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one datagram RECEIVED on `face` (the recv loop's arrival socket).
    pub fn record_face_ingress(&self, face: Face) {
        self.face_messages[face as usize][Direction::Inbound as usize]
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Count one datagram EGRESSED on `face` (the destination-picked socket).
    pub fn record_face_egress(&self, face: Face) {
        self.face_messages[face as usize][Direction::Outbound as usize]
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Read one per-face counter (test/assertion surface).
    pub fn face_messages_total(&self, face: Face, direction: Direction) -> u64 {
        self.face_messages[face as usize][direction as usize].load(Ordering::Relaxed)
    }

    pub fn record_routing_decision(&self, kind: RoutingDecisionKind) {
        self.routing_decisions[kind as usize].fetch_add(1, Ordering::Relaxed);
    }

    pub fn observe_routing_duration(&self, seconds: f64) {
        self.routing_duration_count.fetch_add(1, Ordering::Relaxed);
        self.routing_duration_sum_us.fetch_add((seconds * 1_000_000.0) as u64, Ordering::Relaxed);
    }

    pub fn record_hmac_failure(&self, reason: HmacFailureReason) {
        self.hmac_failures[reason as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// Count one locally decided reject by reason, for
    /// `sip_proxy_rejects_total{reason}`. Callers pass strings from a bounded
    /// static set (the emit sites' `&'static str` reasons plus the self-gate's
    /// two reason constants), keeping label cardinality bounded.
    pub fn record_reject(&self, reason: &str) {
        self.rejects.add(&[reason], 1);
    }

    /// Count one CANCEL's lookup of its INVITE's remembered hop, for
    /// `sip_proxy_cancel_lookups_total{outcome}`.
    pub fn record_cancel_lookup(&self, outcome: CancelLookup) {
        self.cancel_lookups.add(&[outcome.as_str()], 1);
    }

    /// Count what one request's cookie decode says about its primary (an
    /// in-dialog request, or a CANCEL following its INVITE's cookie; each
    /// retransmission counts): a backup promotion past a primary that is up
    /// (`sip_proxy_decode_forward_promotions_total{reason}`), or a forward to a
    /// fresh primary for want of a usable backup
    /// (`sip_proxy_fresh_pod_forwards_total`). Only the request path calls it.
    pub fn record_request_decode(&self, decoded: &DecodeResult) {
        match decoded {
            DecodeResult::ForwardBackup { promotion: Some(reason), .. } => {
                self.decode_forward_promotions.add(&[reason.as_str()], 1);
            }
            DecodeResult::Forward { fresh_primary: true, .. } => {
                self.fresh_pod_forwards.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    pub fn record_overload_rejection(&self, reason: &str) {
        self.overload_rejections.add(&[reason], 1);
    }

    /// Add `n` `sweep_stale` floor events (one per worker the sweep just
    /// conservatively decreased) to the coarse aggregate counter. The runner's
    /// 1 s sweep task calls this with the per-sweep floored count.
    pub fn record_overload_stale_decrease(&self, n: u64) {
        self.overload_stale_decrease.fetch_add(n, Ordering::Relaxed);
    }

    /// Count one emergency new-dialog INVITE that bypassed the LB's overload
    /// gates (critical-filter + AIMD bucket), for
    /// `sip_proxy_lb_emergency_bypassed_total`.
    pub fn record_lb_emergency_bypass(&self) {
        self.lb_emergency_bypassed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_route_inserted(&self) {
        self.record_route_inserted.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_pending_invite_lru_size(&self, n: u64) {
        self.pending_invite_lru_size.store(n, Ordering::Relaxed);
    }

    /// Count a named-target send by outcome (cached/resolved/dropped_*…), for
    /// `sip_proxy_named_sends_total{outcome}`. See [`crate::resolver`].
    pub fn record_named_send(&self, outcome: &str) {
        count_outcome(&self.named_sends, &outcome::ALL, outcome);
    }

    /// Count a proactive resolver refresh / startup prewarm event by outcome
    /// (refreshed/failed/idle_stopped/prewarmed/prewarm_failed), for
    /// `sip_proxy_resolver_refresh_total{outcome}`. See [`crate::resolver`].
    pub fn record_resolver_refresh(&self, outcome: &str) {
        count_outcome(&self.resolver_refresh, &refresh_outcome::ALL, outcome);
    }

    pub fn set_resolver_cache_size(&self, n: u64) {
        self.resolver_cache_size.store(n, Ordering::Relaxed);
    }

    pub fn record_send_failure(&self) {
        self.send_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one per-peer failure of `kind` against `peer` in `scope`
    /// (`sip_proxy_peer_failures_total{peer,scope,kind}`; cardinality-bounded,
    /// see [`crate::observability::peer_failures::PeerFailures`]).
    pub fn record_peer_failure(
        &self,
        peer: &std::net::SocketAddr,
        scope: crate::observability::peer_failures::PeerScope,
        kind: crate::observability::peer_failures::PeerFailureKind,
    ) {
        self.per_peer.record(peer, scope, kind);
    }

    /// Publish one recv shard's endpoint receive-queue state (gauges) and
    /// lifetime counters (monotonic, endpoint-owned — stored, not accumulated).
    /// Single-socket deployments are just `shard = 0`.
    pub fn set_udp_endpoint_stats(&self, shard: usize, stats: UdpShardStats) {
        self.udp_shards.lock().unwrap().insert(shard, stats);
    }

    /// Cross-shard aggregate.
    fn udp_totals(&self) -> UdpShardStats {
        let mut t = UdpShardStats::default();
        for v in self.udp_shards.lock().unwrap().values() {
            t += *v;
        }
        t
    }

    /// Set the worker-health gauges from a fleet count. Also derives
    /// `worker_pool_empty` = `1` iff no worker is `Alive` (routable) — the
    /// routing-fatal condition the `/readyz` gate also keys on.
    pub fn set_worker_health_counts(
        &self,
        alive: u64,
        draining: u64,
        not_ready: u64,
        unknown: u64,
        dead: u64,
    ) {
        self.health.alive.store(alive, Ordering::Relaxed);
        self.health.draining.store(draining, Ordering::Relaxed);
        self.health.not_ready.store(not_ready, Ordering::Relaxed);
        self.health.unknown.store(unknown, Ordering::Relaxed);
        self.health.dead.store(dead, Ordering::Relaxed);
        self.worker_pool_empty.store(u64::from(alive == 0), Ordering::Relaxed);
    }

    /// Publish how many recv shards are stalled (the readiness gate's reading).
    pub fn set_recv_shards_stalled(&self, n: u64) {
        self.recv_shards_stalled.store(n, Ordering::Relaxed);
    }

    // --- read helpers (tests) ---
    pub fn messages_total(&self) -> u64 {
        self.messages.iter().flatten().map(|c| c.load(Ordering::Relaxed)).sum()
    }
    pub fn routing_decisions_total(&self) -> u64 {
        self.routing_decisions.iter().map(|c| c.load(Ordering::Relaxed)).sum()
    }
    pub fn routing_duration_count(&self) -> u64 {
        self.routing_duration_count.load(Ordering::Relaxed)
    }
    pub fn record_route_inserted_total(&self) -> u64 {
        self.record_route_inserted.load(Ordering::Relaxed)
    }
    pub fn pending_invite_lru_size(&self) -> u64 {
        self.pending_invite_lru_size.load(Ordering::Relaxed)
    }
    pub fn calls_total(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
    pub fn named_send_count(&self, outcome: &str) -> u64 {
        outcome::ALL
            .iter()
            .position(|o| *o == outcome)
            .map_or(0, |i| self.named_sends[i].load(Ordering::Relaxed))
    }
    pub fn resolver_refresh_count(&self, outcome: &str) -> u64 {
        refresh_outcome::ALL
            .iter()
            .position(|o| *o == outcome)
            .map_or(0, |i| self.resolver_refresh[i].load(Ordering::Relaxed))
    }
    pub fn resolver_cache_size(&self) -> u64 {
        self.resolver_cache_size.load(Ordering::Relaxed)
    }
    pub fn overload_stale_decrease_total(&self) -> u64 {
        self.overload_stale_decrease.load(Ordering::Relaxed)
    }
    pub fn lb_emergency_bypassed_total(&self) -> u64 {
        self.lb_emergency_bypassed.load(Ordering::Relaxed)
    }
    pub fn overload_rejection_count(&self, reason: &str) -> u64 {
        self.overload_rejections.get(&[reason])
    }
    pub fn reject_count(&self, reason: &str) -> u64 {
        self.rejects.get(&[reason])
    }

    /// Render Prometheus text exposition (the `/metrics` body): every
    /// family of [`catalogue::PROXY`], in order.
    pub fn prometheus_text(&self) -> String {
        use catalogue as c;
        let mut s = String::new();
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        c::MESSAGES.render(&mut s, |series| {
            let (d, r) =
                (series.at(0) / MessageResult::ALL.len(), series.at(0) % MessageResult::ALL.len());
            load(&self.messages[d][r])
        });
        c::FACE_MESSAGES.render(&mut s, |series| {
            let (f, d) = (series.at(0) / Direction::ALL.len(), series.at(0) % Direction::ALL.len());
            load(&self.face_messages[f][d])
        });
        let native = Method::NATIVE_TOKENS
            .iter()
            .zip(&self.requests)
            .map(|(m, n)| (vec![m.to_string()], load(n)));
        c::REQUESTS.render_rows(&mut s, native.chain(self.request_extensions.rows()));
        c::REQUESTS_OVERFLOW.render_value(&mut s, self.request_extensions.overflowed());
        self.responses.render(&mut s);
        c::CALLS.render_value(&mut s, load(&self.calls));
        c::ROUTING_DECISION.render(&mut s, |series| load(&self.routing_decisions[series.at(0)]));
        self.rejects.render(&mut s);
        c::HMAC_FAILURES.render(&mut s, |series| load(&self.hmac_failures[series.at(0)]));
        self.cancel_lookups.render(&mut s);
        self.decode_forward_promotions.render(&mut s);
        c::FRESH_POD_FORWARDS.render_value(&mut s, load(&self.fresh_pod_forwards));
        // A histogram without finite buckets: the count and the sum.
        let count = load(&self.routing_duration_count);
        let sum = load(&self.routing_duration_sum_us) as f64 / 1_000_000.0;
        c::ROUTING_DURATION_SECONDS.render_histogram(&mut s, |_| HistogramValue {
            buckets: Vec::new(),
            sum,
            count,
        });
        c::RECORD_ROUTE_INSERTED.render_value(&mut s, load(&self.record_route_inserted));
        c::PENDING_INVITE_LRU_SIZE.render_value(&mut s, load(&self.pending_invite_lru_size));
        c::NAMED_SENDS.render(&mut s, |series| load(&self.named_sends[series.at(0)]));
        c::RESOLVER_REFRESH.render(&mut s, |series| load(&self.resolver_refresh[series.at(0)]));
        c::RESOLVER_CACHE_SIZE.render_value(&mut s, load(&self.resolver_cache_size));
        c::SEND_FAILURES.render_value(&mut s, load(&self.send_failures));
        let udp = self.udp_totals();
        c::UDP_QUEUE_DEPTH.render_value(&mut s, udp.queue_depth);
        c::UDP_QUEUE_MAX.render_value(&mut s, udp.queue_max);
        c::UDP_ENQUEUED.render_value(&mut s, udp.enqueued);
        c::UDP_TAIL_DROPPED.render_value(&mut s, udp.tail_dropped);
        c::INTAKE_SHED.render_value(&mut s, udp.intake_shed);
        c::UDP_SEND_WOULD_BLOCK.render_value(&mut s, udp.send_would_block);
        c::UDP_KERNEL_RX_DROPPED.render_value(&mut s, udp.kernel_rx_dropped);
        c::RECV_SHARDS_STALLED.render_value(&mut s, load(&self.recv_shards_stalled));
        c::WORKER_POOL_EMPTY.render_value(&mut s, load(&self.worker_pool_empty));

        // Overload-shed visibility (port of the TS AIMD counters). Without these a
        // worker that gets silently rate-capped (`bucket_empty`), filtered out
        // (`no_target_critical_filtered`), or floored on stale telemetry
        // (`stale_decrease`) leaves no Prometheus trail. `overload_rejections` is
        // keyed by reason; `stale_decrease` is the coarse (registry-aggregate)
        // stand-in for a per-`worker_id` counter.
        self.overload_rejections.render(&mut s);
        c::WORKER_STALE_DECREASE.render_value(&mut s, load(&self.overload_stale_decrease));
        c::LB_EMERGENCY_BYPASSED.render_value(&mut s, load(&self.lb_emergency_bypassed));

        let health = [
            &self.health.alive,
            &self.health.draining,
            &self.health.not_ready,
            &self.health.unknown,
            &self.health.dead,
        ];
        c::WORKER_HEALTH.render(&mut s, |series| load(health[series.at(0)]));

        // Per-peer failure/timeout family: internal = known worker
        // (registry), external = under the cap.
        self.per_peer.render(&mut s);
        s
    }
}

/// The proxy-self gate's families: its gauges and its admission counts;
/// without a gate (the always-admit one) the gauges read `NaN` and the
/// counters 0.
pub fn self_gate_text(gate: Option<&crate::self_gate::ProxySelfGateMetrics>) -> String {
    use catalogue as c;
    let mut s = String::new();
    let gauge = |v: fn(&crate::self_gate::ProxySelfGateMetrics) -> f64| gate.map_or(f64::NAN, v);
    let count = |v: fn(&crate::self_gate::ProxySelfGateMetrics) -> u64| gate.map_or(0, v);
    c::SELF_GATE_ELU_EWMA.render_value(&mut s, gauge(|m| m.elu_ewma));
    c::SELF_GATE_GC_FRACTION.render_value(&mut s, gauge(|m| m.gc_fraction));
    c::SELF_GATE_CPS_BUCKET_LEVEL.render_value(&mut s, gauge(|m| m.cps_bucket_level));
    c::SELF_GATE_CPS_BUCKET_MAX.render_value(&mut s, gauge(|m| m.cps_bucket_max));
    c::SELF_GATE_EXTERNAL_INVITES_ADMITTED
        .render_value(&mut s, count(|m| m.external_admitted_total));
    let rejected = [count(|m| m.rejected_elu_total), count(|m| m.rejected_cps_total)];
    c::SELF_GATE_EXTERNAL_INVITES_REJECTED.render(&mut s, |series| rejected[series.at(0)]);
    c::SELF_GATE_EMERGENCY_BYPASSED.render_value(&mut s, count(|m| m.emergency_bypassed_total));
    c::SELF_GATE_INTERNAL_BYPASSED.render_value(&mut s, count(|m| m.internal_bypassed_total));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shard's stats sum its two faces: the external face, where callers
    /// arrive, is not invisible in any series.
    #[test]
    fn a_dual_face_shard_sums_both_faces() {
        use sip_net::UdpEndpointCounters;

        struct Face(usize, UdpEndpointCounters);
        #[async_trait::async_trait]
        impl UdpEndpoint for Face {
            async fn send_to(
                &self,
                _buf: &[u8],
                _dst: std::net::SocketAddr,
            ) -> Result<(), sip_net::SendError> {
                Ok(())
            }
            async fn recv(&self) -> Option<sip_net::UdpPacket> {
                None
            }
            fn try_recv(&self) -> Option<sip_net::UdpPacket> {
                None
            }
            fn local_addr(&self) -> std::net::SocketAddr {
                "127.0.0.1:5060".parse().unwrap()
            }
            fn queue_depth(&self) -> usize {
                self.0
            }
            fn queue_max(&self) -> usize {
                8
            }
            fn counters(&self) -> UdpEndpointCounters {
                self.1
            }
        }
        let face = |n: u64| {
            Face(
                n as usize,
                UdpEndpointCounters {
                    enqueued: n,
                    tail_dropped: n,
                    pre_ingress_dropped: n,
                    send_would_block: n,
                    kernel_rx_dropped: n,
                    ..UdpEndpointCounters::default()
                },
            )
        };
        let (int, ext) = (face(1), face(10));
        let each = |v| UdpShardStats {
            queue_depth: v,
            queue_max: 16,
            enqueued: v,
            tail_dropped: v,
            intake_shed: v,
            send_would_block: v,
            kernel_rx_dropped: v,
        };
        assert_eq!(UdpShardStats::of_faces(&int, Some(&ext)), each(11));
        assert_eq!(UdpShardStats::of_faces(&int, None), UdpShardStats { queue_max: 8, ..each(1) });
    }

    /// Each shard's endpoint stats render summed over the shards; the kernel
    /// drop count is its own counter beside the queue's tail drops.
    #[test]
    fn udp_shard_stats_render_summed_over_shards() {
        let m = ProxyMetrics::new();
        let shard = |kernel_rx_dropped| UdpShardStats {
            queue_max: 8,
            tail_dropped: 1,
            kernel_rx_dropped,
            ..UdpShardStats::default()
        };
        m.set_udp_endpoint_stats(0, shard(3));
        m.set_udp_endpoint_stats(1, shard(4));
        m.set_udp_endpoint_stats(1, shard(5));

        let txt = m.prometheus_text();
        assert!(txt.contains("\nsip_proxy_udp_queue_max 16\n"));
        assert!(txt.contains("\nsip_proxy_udp_tail_dropped_total 2\n"));
        assert!(txt.contains("# TYPE sip_proxy_udp_kernel_rx_dropped_total counter"));
        assert!(txt.contains("\nsip_proxy_udp_kernel_rx_dropped_total 8\n"));
    }

    #[test]
    fn counters_move_and_render() {
        let m = ProxyMetrics::new();
        m.record_message(Direction::Inbound, MessageResult::Forwarded);
        m.record_message(Direction::Outbound, MessageResult::Forwarded);
        m.record_routing_decision(RoutingDecisionKind::SelectNew);
        m.observe_routing_duration(0.0005);
        m.record_route_inserted();
        assert_eq!(m.messages_total(), 2);
        assert_eq!(m.routing_decisions_total(), 1);
        assert_eq!(m.routing_duration_count(), 1);
        assert_eq!(m.record_route_inserted_total(), 1);

        let txt = m.prometheus_text();
        assert!(txt.contains("# TYPE sip_messages_total counter"));
        assert!(txt.contains("# TYPE sip_routing_duration_seconds histogram"));
        assert!(txt.contains("sip_routing_duration_seconds_count 1"));
        assert!(txt.contains("# TYPE sip_worker_health gauge"));
    }

    #[test]
    fn per_method_request_response_and_calls_render() {
        let m = ProxyMetrics::new();
        m.record_request("INVITE");
        m.record_request("BYE");
        m.record_response("INVITE", 200);
        m.record_response("INVITE", 487);
        m.record_call();
        let txt = m.prometheus_text();
        assert!(txt.contains("sip_proxy_requests_total{method=\"INVITE\"} 1"));
        assert!(txt.contains("sip_proxy_requests_total{method=\"BYE\"} 1"));
        assert!(txt.contains("sip_proxy_responses_total{method=\"INVITE\",code=\"200\"} 1"));
        assert!(txt.contains("sip_proxy_responses_total{method=\"INVITE\",code=\"487\"} 1"));
        assert!(txt.contains("sip_proxy_calls_total 1"));
    }

    #[test]
    fn overload_shed_counters_render() {
        // Both the keyed rejection counter and the coarse stale-decrease aggregate
        // must surface in the exposition — a silently rate-capped or floored worker
        // is otherwise invisible in Prometheus (the minor-comment fix).
        let m = ProxyMetrics::new();
        m.record_overload_rejection("bucket_empty");
        m.record_overload_rejection("bucket_empty");
        m.record_overload_rejection("no_target_critical_filtered");
        m.record_overload_stale_decrease(3);
        m.record_lb_emergency_bypass();
        m.record_lb_emergency_bypass();
        assert_eq!(m.overload_rejection_count("bucket_empty"), 2);
        assert_eq!(m.overload_stale_decrease_total(), 3);
        assert_eq!(m.lb_emergency_bypassed_total(), 2);
        let txt = m.prometheus_text();
        assert!(txt.contains("sip_proxy_overload_rejections_total{reason=\"bucket_empty\"} 2"));
        assert!(txt.contains(
            "sip_proxy_overload_rejections_total{reason=\"no_target_critical_filtered\"} 1"
        ));
        assert!(txt.contains("sip_proxy_worker_stale_decrease_total 3"));
        // Emergency-bypass visibility series (the LB critical-filter + AIMD skip).
        assert!(txt.contains("sip_proxy_lb_emergency_bypassed_total 2"));
        assert!(txt.contains("# TYPE sip_proxy_lb_emergency_bypassed_total counter"));
    }

    #[test]
    fn reject_reason_counter_renders() {
        // Every reason must surface as its own labelled series — the aggregate
        // kind="reject" bucket alone leaves a locally generated 503
        // unattributable from metrics.
        let m = ProxyMetrics::new();
        m.record_reject("worker_rate_capped");
        m.record_reject("worker_rate_capped");
        m.record_reject("no_target_available");
        m.record_reject("too_many_hops");
        assert_eq!(m.reject_count("worker_rate_capped"), 2);
        assert_eq!(m.reject_count("no_target_available"), 1);
        let txt = m.prometheus_text();
        assert!(txt.contains("# TYPE sip_proxy_rejects_total counter"));
        assert!(txt.contains("sip_proxy_rejects_total{reason=\"worker_rate_capped\"} 2"));
        assert!(txt.contains("sip_proxy_rejects_total{reason=\"no_target_available\"} 1"));
        assert!(txt.contains("sip_proxy_rejects_total{reason=\"too_many_hops\"} 1"));
    }

    #[test]
    fn worker_health_gauges_render() {
        let m = ProxyMetrics::new();
        m.set_worker_health_counts(1, 0, 0, 0, 0);
        let txt = m.prometheus_text();
        assert!(txt.contains("sip_worker_health{health=\"alive\"} 1"));
        assert!(txt.contains("sip_worker_health{health=\"draining\"} 0"));
    }

    #[test]
    fn wire_controlled_labels_are_bounded() {
        // A flood of invented methods must not grow label cardinality without
        // bound: each gets its own series up to the cap, the rest land on the
        // overflow series and are counted (remote memory-exhaustion guard).
        use metric_catalogue::{DEFAULT_CAP, OVERFLOW};
        let m = ProxyMetrics::new();
        for i in 0..1_000 {
            m.record_request(&format!("FOO{i}"));
            m.record_response(&format!("BAR{i}"), 299);
        }
        let txt = m.prometheus_text();
        let series = |prefix: &str| txt.lines().filter(|l| l.starts_with(prefix)).count();
        assert_eq!(series("sip_proxy_requests_total{method=\"FOO"), DEFAULT_CAP);
        let past = 1_000 - DEFAULT_CAP;
        assert!(
            txt.contains(&format!("sip_proxy_requests_total{{method=\"{OVERFLOW}\"}} {past}\n"))
        );
        assert!(txt.contains(&format!("\nsip_proxy_requests_overflow_total {past}\n")));
        assert_eq!(series("sip_proxy_responses_total{method=\"BAR"), DEFAULT_CAP);
        assert!(txt.contains(&format!("\nsip_proxy_responses_overflow_total {past}\n")));
        assert!(txt.contains("sip_proxy_requests_total{method=\"FOO0\"} 1\n"), "{txt}");
        assert!(!txt.contains("FOO999"), "a token past the cap gets no series of its own");
    }
}
