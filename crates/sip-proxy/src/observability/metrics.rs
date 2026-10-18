//! [`ProxyMetrics`] — atomics-backed counters/gauges for the proxy data path:
//! live atomics + small labeled maps, rendered as Prometheus text
//! ([`ProxyMetrics::prometheus_text`]) for the `/metrics` endpoint.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use metric_catalogue::{FixedCounts, HistogramValue, OpenRows};
use sip_message::method::Method;

use super::catalogue;
use pool::PoolGauges;

mod face;
mod no_target;
mod pool;
mod udp_shard;
use crate::resolver::{outcome, refresh_outcome};
pub use face::{Face, UntrustedClaim};
pub use no_target::NoTargetDrop;
pub use udp_shard::UdpShardStats;

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
    no_target_dropped: FixedCounts,
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
    /// Inbound datagrams the parser panicked on and a recv shard dropped.
    parse_panics: AtomicU64,
    /// Endpoint receive-queue stats, one slot per recv shard, published by
    /// each core's maintenance tick — without them a tail-dropping queue shows
    /// 100% forwarded. Keyed by shard index so N reuse-port cores don't stomp
    /// one gauge; rendered as the cross-shard aggregate (sum). Not hot:
    /// written every sweep tick, read at render.
    udp_shards: Mutex<BTreeMap<usize, UdpShardStats>>,
    /// The worker pool: health counts, empty, discovery health.
    pool: PoolGauges,
    /// Recv shards currently past the stall threshold (see `liveness`).
    recv_shards_stalled: AtomicU64,
    /// Per-face traffic counters (dual-face mode): `[face][direction]` where
    /// face ∈ {int, ext} and direction ∈ {inbound (recv), outbound (egress)}.
    /// A single-face proxy only ever touches the `int` slots — cheap fixed
    /// atomics, no restructuring of the existing counters.
    face_messages: [[AtomicU64; 2]; 2],
    /// Claims of the internal plane the external face does not honour.
    external_untrusted: FixedCounts,
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
            no_target_dropped: FixedCounts::new(&catalogue::NO_TARGET_DROPPED),
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
            parse_panics: Default::default(),
            udp_shards: Default::default(),
            pool: Default::default(),
            recv_shards_stalled: Default::default(),
            face_messages: Default::default(),
            external_untrusted: FixedCounts::new(&catalogue::EXTERNAL_UNTRUSTED),
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

    /// One inbound datagram the parser panicked on, dropped.
    pub fn record_parse_panic(&self) {
        self.parse_panics.fetch_add(1, Ordering::Relaxed);
    }

    /// Inbound datagrams the parser panicked on (counter).
    pub fn parse_panics(&self) -> u64 {
        self.parse_panics.load(Ordering::Relaxed)
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
        self.udp_shards.lock().unwrap().values().copied().sum()
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
        self.external_untrusted.render(&mut s);
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
        self.no_target_dropped.render(&mut s);
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
        c::PARSE_PANICS.render_value(&mut s, load(&self.parse_panics));
        let udp = self.udp_totals();
        c::UDP_QUEUE_DEPTH.render_value(&mut s, udp.queue_depth);
        c::UDP_QUEUE_MAX.render_value(&mut s, udp.queue_max);
        c::UDP_ENQUEUED.render_value(&mut s, udp.enqueued);
        c::UDP_TAIL_DROPPED.render_value(&mut s, udp.tail_dropped);
        c::INTAKE_SHED.render_value(&mut s, udp.intake_shed);
        c::UDP_SEND_WOULD_BLOCK.render_value(&mut s, udp.send_would_block);
        c::UDP_KERNEL_RX_DROPPED.render_value(&mut s, udp.kernel_rx_dropped);
        c::UDP_PRE_INGRESS_PANICS.render_value(&mut s, udp.pre_ingress_panics);
        c::RECV_SHARDS_STALLED.render_value(&mut s, load(&self.recv_shards_stalled));
        self.pool.render_pool(&mut s);

        // Overload-shed visibility. Without these a
        // worker that gets silently rate-capped (`bucket_empty`), filtered out
        // (`no_target_critical_filtered`), or floored on stale telemetry
        // (`stale_decrease`) leaves no Prometheus trail. `overload_rejections` is
        // keyed by reason; `stale_decrease` is the coarse (registry-aggregate)
        // stand-in for a per-`worker_id` counter.
        self.overload_rejections.render(&mut s);
        c::WORKER_STALE_DECREASE.render_value(&mut s, load(&self.overload_stale_decrease));
        c::LB_EMERGENCY_BYPASSED.render_value(&mut s, load(&self.lb_emergency_bypassed));

        self.pool.render_health(&mut s);

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
    let rejected = [count(|m| m.rejected_intake_age_total), count(|m| m.rejected_cps_total)];
    c::SELF_GATE_EXTERNAL_INVITES_REJECTED.render(&mut s, |series| rejected[series.at(0)]);
    c::SELF_GATE_EMERGENCY_BYPASSED.render_value(&mut s, count(|m| m.emergency_bypassed_total));
    c::SELF_GATE_INTERNAL_BYPASSED.render_value(&mut s, count(|m| m.internal_bypassed_total));
    s
}

#[cfg(test)]
mod tests;
