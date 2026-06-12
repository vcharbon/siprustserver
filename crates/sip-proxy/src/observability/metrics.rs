//! [`ProxyMetrics`] — atomics-backed counters/gauges for the proxy data path
//! (port of `observability/Metrics.ts`). The source uses Effect `Metric`s; here
//! we keep live atomics + small labeled maps and render Prometheus text
//! ([`ProxyMetrics::prometheus_text`]) for the [`super::metrics_server`] endpoint.
//!
//! Mirrors the source metric names so dashboards transfer: `sip_messages_total`,
//! `sip_routing_decision_total`, `sip_routing_duration_seconds` (histogram),
//! `sip_proxy_hmac_failures_total`, `sip_worker_health`, etc.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Inbound vs outbound, for `sip_messages_total{direction}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Inbound,
    Outbound,
}

impl Direction {
    const ALL: [Direction; 2] = [Direction::Inbound, Direction::Outbound];

    fn as_str(self) -> &'static str {
        match self {
            Direction::Inbound => "inbound",
            Direction::Outbound => "outbound",
        }
    }
}

/// How a message was handled, for `sip_messages_total{result}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageResult {
    Forwarded,
    Responded,
    Dropped,
}

impl MessageResult {
    const ALL: [MessageResult; 3] = [MessageResult::Forwarded, MessageResult::Responded, MessageResult::Dropped];

    fn as_str(self) -> &'static str {
        match self {
            MessageResult::Forwarded => "forwarded",
            MessageResult::Responded => "responded",
            MessageResult::Dropped => "dropped",
        }
    }
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
    Reject,
}

impl RoutingDecisionKind {
    const ALL: [RoutingDecisionKind; 7] = [
        RoutingDecisionKind::SelectNew,
        RoutingDecisionKind::DecodeForward,
        RoutingDecisionKind::DecodeForwardBackup,
        RoutingDecisionKind::LooseRoute,
        RoutingDecisionKind::WorkerOutbound,
        RoutingDecisionKind::Cancel,
        RoutingDecisionKind::Reject,
    ];

    fn as_str(self) -> &'static str {
        match self {
            RoutingDecisionKind::SelectNew => "select_new",
            RoutingDecisionKind::DecodeForward => "decode_forward",
            RoutingDecisionKind::DecodeForwardBackup => "decode_forward_backup",
            RoutingDecisionKind::LooseRoute => "loose_route",
            RoutingDecisionKind::WorkerOutbound => "worker_outbound",
            RoutingDecisionKind::Cancel => "cancel",
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
    fn as_str(self) -> &'static str {
        match self {
            HmacFailureReason::Missing => "missing",
            HmacFailureReason::Decode => "decode",
            HmacFailureReason::Mismatch => "mismatch",
        }
    }
}

/// Known request methods get fixed counter slots; anything else lands in
/// `other`. The method token is wire-controlled and the lenient parser accepts
/// any token, so an open label set was a remote memory-exhaustion vector: a
/// flood of invented methods (`FOO1`, `FOO2`, …) grew one permanent map entry
/// + one `/metrics` line per distinct token, ballooning RSS and Prometheus
/// cardinality without bound.
const METHOD_SLOTS: [&str; 14] = [
    "INVITE", "ACK", "BYE", "CANCEL", "OPTIONS", "REGISTER", "SUBSCRIBE", "NOTIFY", "PRACK", "UPDATE", "INFO",
    "MESSAGE", "REFER", "PUBLISH",
];

/// Slot index for an (uppercased) method token; unknown → the `other` slot.
fn method_slot(method: &str) -> usize {
    METHOD_SLOTS.iter().position(|m| *m == method).unwrap_or(METHOD_SLOTS.len())
}

fn method_label(slot: usize) -> &'static str {
    METHOD_SLOTS.get(slot).copied().unwrap_or("other")
}

#[derive(Default)]
struct LabeledCounter(Mutex<BTreeMap<String, u64>>);

impl LabeledCounter {
    fn inc(&self, label: &str) {
        *self.0.lock().unwrap().entry(label.to_string()).or_insert(0) += 1;
    }
    fn sum(&self) -> u64 {
        self.0.lock().unwrap().values().sum()
    }
    fn snapshot(&self) -> BTreeMap<String, u64> {
        self.0.lock().unwrap().clone()
    }
}

/// The five worker-health gauges (one set per registry — the source keys per
/// worker; the slice's tests assert the aggregate, so we count workers in each
/// health state).
#[derive(Default)]
struct HealthGauges {
    alive: AtomicU64,
    draining: AtomicU64,
    not_ready: AtomicU64,
    unknown: AtomicU64,
    dead: AtomicU64,
}

/// Live proxy metrics. Cheap to share behind an `Arc`.
#[derive(Default)]
pub struct ProxyMetrics {
    /// `[direction][result]` — fixed slots, lock-free (multiple increments per
    /// packet on the hot path).
    messages: [[AtomicU64; 3]; 2],
    /// One slot per known method + `other` — bounded and lock-free.
    requests: [AtomicU64; METHOD_SLOTS.len() + 1],
    /// Keyed "method|code" with the method slotted and the code validated to
    /// 100..699 (else `other`) — both wire-controlled inputs bounded.
    responses: LabeledCounter,
    calls: AtomicU64,                // initial (dialog-creating, no To-tag) INVITEs
    routing_decisions: [AtomicU64; 7], // indexed by RoutingDecisionKind
    hmac_failures: LabeledCounter,   // keyed reason
    cancel_lookups: LabeledCounter,  // keyed outcome
    decode_forward_promoted: LabeledCounter, // keyed from-reason
    fresh_pod_forward: LabeledCounter, // keyed age-bucket
    overload_rejections: LabeledCounter, // keyed reason
    routing_duration_count: AtomicU64,
    routing_duration_sum_us: AtomicU64,
    record_route_inserted: AtomicU64,
    ack_synthesized: AtomicU64,
    pending_invite_lru_size: AtomicU64,
    named_sends: LabeledCounter, // keyed outcome (crate::resolver::outcome — closed set)
    resolver_cache_size: AtomicU64,
    /// Outbound datagrams the endpoint failed to send (EPERM/ENOBUFS/...).
    /// `sip_messages_total{outbound,forwarded}` counts hand-off to the send
    /// path, so this is the delta dashboards need under overload.
    send_failures: AtomicU64,
    /// Endpoint receive-queue counters, published by the core's maintenance
    /// tick — without them a tail-dropping queue shows 100% forwarded (the
    /// blind spot that hid the 2026-06-12 burst collapse from the gates).
    udp_queue_depth: AtomicU64,
    udp_queue_max: AtomicU64,
    udp_enqueued: AtomicU64,
    udp_tail_dropped: AtomicU64,
    health: HealthGauges,
    /// `1` ⇒ the worker pool has **zero routable (`Alive`) workers** — the proxy
    /// can serve no new dialog. Set from the registry by the runner's health
    /// sampler (ADR-0012 D4): an empty/RBAC-forbidden EndpointSlice informer pool
    /// is otherwise silent (the proxy just black-holes every INVITE). Pairs with
    /// the `/readyz` gate; alert on `sip_proxy_worker_pool_empty == 1`.
    worker_pool_empty: AtomicU64,
}

impl ProxyMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_message(&self, direction: Direction, result: MessageResult) {
        self.messages[direction as usize][result as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// Count one inbound request by SIP method (uppercased by the caller), for
    /// `sip_proxy_requests_total{method}`. Unknown methods share one `other`
    /// slot — see [`METHOD_SLOTS`].
    pub fn record_request(&self, method: &str) {
        self.requests[method_slot(method)].fetch_add(1, Ordering::Relaxed);
    }

    /// Count one inbound response by its CSeq method + status code, for
    /// `sip_proxy_responses_total{method,code}`. Both labels are bounded:
    /// unknown methods → `other`, out-of-range codes → `other`.
    pub fn record_response(&self, method: &str, code: u16) {
        let method = method_label(method_slot(method));
        if (100..700).contains(&code) {
            self.responses.inc(&format!("{method}|{code}"));
        } else {
            self.responses.inc(&format!("{method}|other"));
        }
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
        self.hmac_failures.inc(reason.as_str());
    }

    pub fn record_cancel_lookup(&self, outcome: &str) {
        self.cancel_lookups.inc(outcome);
    }

    pub fn record_decode_forward_promoted(&self, from: &str) {
        self.decode_forward_promoted.inc(from);
    }

    pub fn record_fresh_pod_forward(&self, bucket: &str) {
        self.fresh_pod_forward.inc(bucket);
    }

    pub fn record_overload_rejection(&self, reason: &str) {
        self.overload_rejections.inc(reason);
    }

    pub fn record_route_inserted(&self) {
        self.record_route_inserted.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_ack_synthesized(&self) {
        self.ack_synthesized.fetch_add(1, Ordering::Relaxed);
    }

    pub fn set_pending_invite_lru_size(&self, n: u64) {
        self.pending_invite_lru_size.store(n, Ordering::Relaxed);
    }

    /// Count a named-target send by outcome (cached/resolved/dropped_*…), for
    /// `sip_proxy_named_sends_total{outcome}`. See [`crate::resolver`].
    pub fn record_named_send(&self, outcome: &str) {
        self.named_sends.inc(outcome);
    }

    pub fn set_resolver_cache_size(&self, n: u64) {
        self.resolver_cache_size.store(n, Ordering::Relaxed);
    }

    pub fn record_send_failure(&self) {
        self.send_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Publish the endpoint's receive-queue state (gauges) and lifetime
    /// counters (monotonic, endpoint-owned — stored, not accumulated).
    pub fn set_udp_endpoint_stats(&self, queue_depth: u64, queue_max: u64, enqueued: u64, tail_dropped: u64) {
        self.udp_queue_depth.store(queue_depth, Ordering::Relaxed);
        self.udp_queue_max.store(queue_max, Ordering::Relaxed);
        self.udp_enqueued.store(enqueued, Ordering::Relaxed);
        self.udp_tail_dropped.store(tail_dropped, Ordering::Relaxed);
    }

    /// Set the worker-health gauges from a fleet count. Also derives
    /// `worker_pool_empty` = `1` iff no worker is `Alive` (routable) — the
    /// routing-fatal condition the `/readyz` gate also keys on.
    pub fn set_worker_health_counts(&self, alive: u64, draining: u64, not_ready: u64, unknown: u64, dead: u64) {
        self.health.alive.store(alive, Ordering::Relaxed);
        self.health.draining.store(draining, Ordering::Relaxed);
        self.health.not_ready.store(not_ready, Ordering::Relaxed);
        self.health.unknown.store(unknown, Ordering::Relaxed);
        self.health.dead.store(dead, Ordering::Relaxed);
        self.worker_pool_empty.store(u64::from(alive == 0), Ordering::Relaxed);
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
    pub fn hmac_failures_total(&self) -> u64 {
        self.hmac_failures.sum()
    }
    pub fn record_route_inserted_total(&self) -> u64 {
        self.record_route_inserted.load(Ordering::Relaxed)
    }
    pub fn ack_synthesized_total(&self) -> u64 {
        self.ack_synthesized.load(Ordering::Relaxed)
    }
    pub fn pending_invite_lru_size(&self) -> u64 {
        self.pending_invite_lru_size.load(Ordering::Relaxed)
    }
    pub fn calls_total(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
    pub fn named_send_count(&self, outcome: &str) -> u64 {
        self.named_sends.snapshot().get(outcome).copied().unwrap_or(0)
    }
    pub fn resolver_cache_size(&self) -> u64 {
        self.resolver_cache_size.load(Ordering::Relaxed)
    }
    pub fn send_failures_total(&self) -> u64 {
        self.send_failures.load(Ordering::Relaxed)
    }
    pub fn udp_tail_dropped_total(&self) -> u64 {
        self.udp_tail_dropped.load(Ordering::Relaxed)
    }

    /// Render Prometheus text exposition (the `/metrics` body).
    pub fn prometheus_text(&self) -> String {
        let mut s = String::new();
        let g = |s: &mut String, name: &str, help: &str, ty: &str, val: u64| {
            s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {ty}\n{name} {val}\n"));
        };
        let labeled = |s: &mut String, name: &str, help: &str, ty: &str, label: &str, m: &BTreeMap<String, u64>| {
            s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {ty}\n"));
            if m.is_empty() {
                s.push_str(&format!("{name}{{{label}=\"none\"}} 0\n"));
            }
            for (k, v) in m {
                s.push_str(&format!("{name}{{{label}=\"{k}\"}} {v}\n"));
            }
        };

        // Two-label render: key is "method|code" -> {method="..",code=".."}.
        let labeled2 = |s: &mut String, name: &str, help: &str, (l1, l2): (&str, &str), m: &BTreeMap<String, u64>| {
            s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n"));
            if m.is_empty() {
                s.push_str(&format!("{name}{{{l1}=\"none\",{l2}=\"none\"}} 0\n"));
            }
            for (k, v) in m {
                let (a, b) = k.split_once('|').unwrap_or((k.as_str(), ""));
                s.push_str(&format!("{name}{{{l1}=\"{a}\",{l2}=\"{b}\"}} {v}\n"));
            }
        };

        // Slot-backed counters render only their non-zero entries, matching the
        // sparse output the map-backed counters produced.
        let mut messages_map = BTreeMap::new();
        for d in Direction::ALL {
            for r in MessageResult::ALL {
                let v = self.messages[d as usize][r as usize].load(Ordering::Relaxed);
                if v > 0 {
                    messages_map.insert(format!("{}:{}", d.as_str(), r.as_str()), v);
                }
            }
        }
        let mut requests_map = BTreeMap::new();
        for (slot, c) in self.requests.iter().enumerate() {
            let v = c.load(Ordering::Relaxed);
            if v > 0 {
                requests_map.insert(method_label(slot).to_string(), v);
            }
        }
        let mut decisions_map = BTreeMap::new();
        for k in RoutingDecisionKind::ALL {
            let v = self.routing_decisions[k as usize].load(Ordering::Relaxed);
            if v > 0 {
                decisions_map.insert(k.as_str().to_string(), v);
            }
        }

        labeled(&mut s, "sip_messages_total", "SIP messages by direction+result.", "counter", "label", &messages_map);
        labeled(&mut s, "sip_proxy_requests_total", "Inbound SIP requests by method.", "counter", "method", &requests_map);
        labeled2(&mut s, "sip_proxy_responses_total", "Inbound SIP responses by CSeq method + status code.", ("method", "code"), &self.responses.snapshot());
        g(&mut s, "sip_proxy_calls_total", "New calls: initial dialog-creating INVITEs (no To-tag).", "counter", self.calls.load(Ordering::Relaxed));
        labeled(&mut s, "sip_routing_decision_total", "Routing decisions by kind.", "counter", "kind", &decisions_map);
        labeled(&mut s, "sip_proxy_hmac_failures_total", "HMAC verify failures by reason.", "counter", "reason", &self.hmac_failures.snapshot());

        // Histogram (count + sum only — the slice does not bucket).
        let cnt = self.routing_duration_count.load(Ordering::Relaxed);
        let sum_s = self.routing_duration_sum_us.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        s.push_str("# HELP sip_routing_duration_seconds Routing decision duration.\n");
        s.push_str("# TYPE sip_routing_duration_seconds histogram\n");
        s.push_str(&format!("sip_routing_duration_seconds_bucket{{le=\"+Inf\"}} {cnt}\n"));
        s.push_str(&format!("sip_routing_duration_seconds_sum {sum_s}\n"));
        s.push_str(&format!("sip_routing_duration_seconds_count {cnt}\n"));

        g(&mut s, "sip_proxy_record_route_inserted_total", "Record-Route headers inserted.", "counter", self.record_route_inserted.load(Ordering::Relaxed));
        g(&mut s, "sip_proxy_ack_synthesized_total", "Hop-by-hop ACKs synthesized for non-2xx.", "counter", self.ack_synthesized.load(Ordering::Relaxed));
        g(&mut s, "sip_proxy_pending_invite_lru_size", "Pending-INVITE LRU size.", "gauge", self.pending_invite_lru_size.load(Ordering::Relaxed));
        labeled(&mut s, "sip_proxy_named_sends_total", "Named-target (DNS) sends by outcome.", "counter", "outcome", &self.named_sends.snapshot());
        g(&mut s, "sip_proxy_resolver_cache_size", "Resolver name-cache size.", "gauge", self.resolver_cache_size.load(Ordering::Relaxed));
        g(&mut s, "sip_proxy_send_failures_total", "Outbound datagrams the endpoint failed to send.", "counter", self.send_failures.load(Ordering::Relaxed));
        g(&mut s, "sip_proxy_udp_queue_depth", "Inbound UDP queue depth (sampled).", "gauge", self.udp_queue_depth.load(Ordering::Relaxed));
        g(&mut s, "sip_proxy_udp_queue_max", "Inbound UDP queue capacity.", "gauge", self.udp_queue_max.load(Ordering::Relaxed));
        g(&mut s, "sip_proxy_udp_enqueued_total", "Datagrams accepted into the inbound queue.", "counter", self.udp_enqueued.load(Ordering::Relaxed));
        g(&mut s, "sip_proxy_udp_tail_dropped_total", "Datagrams tail-dropped by the full inbound queue.", "counter", self.udp_tail_dropped.load(Ordering::Relaxed));
        g(&mut s, "sip_proxy_worker_pool_empty", "1 iff no worker is Alive (routable) — the proxy can serve no new dialog.", "gauge", self.worker_pool_empty.load(Ordering::Relaxed));

        s.push_str("# HELP sip_worker_health Worker count by health state.\n# TYPE sip_worker_health gauge\n");
        for (label, val) in [
            ("alive", &self.health.alive),
            ("draining", &self.health.draining),
            ("not-ready", &self.health.not_ready),
            ("unknown", &self.health.unknown),
            ("dead", &self.health.dead),
        ] {
            s.push_str(&format!("sip_worker_health{{health=\"{label}\"}} {}\n", val.load(Ordering::Relaxed)));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn worker_health_gauges_render() {
        let m = ProxyMetrics::new();
        m.set_worker_health_counts(1, 0, 0, 0, 0);
        let txt = m.prometheus_text();
        assert!(txt.contains("sip_worker_health{health=\"alive\"} 1"));
        assert!(txt.contains("sip_worker_health{health=\"draining\"} 0"));
    }

    #[test]
    fn wire_controlled_labels_are_bounded() {
        // A flood of invented methods/codes must not grow label cardinality —
        // unknown tokens share the `other` slot (remote memory-exhaustion fix).
        let m = ProxyMetrics::new();
        for i in 0..1_000 {
            m.record_request(&format!("FOO{i}"));
            m.record_response(&format!("BAR{i}"), 9_999);
        }
        let txt = m.prometheus_text();
        assert!(txt.contains("sip_proxy_requests_total{method=\"other\"} 1000"));
        assert!(txt.contains("sip_proxy_responses_total{method=\"other\",code=\"other\"} 1000"));
        assert!(!txt.contains("FOO"), "no per-token label may leak into the exposition");
        // The whole body stays small — one line per slot, not per token.
        assert!(txt.len() < 8_192, "exposition must stay bounded, got {} bytes", txt.len());
    }
}
