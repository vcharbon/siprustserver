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
    messages: LabeledCounter,        // keyed "direction:result"
    routing_decisions: LabeledCounter, // keyed kind
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
    health: HealthGauges,
}

impl ProxyMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_message(&self, direction: Direction, result: MessageResult) {
        self.messages.inc(&format!("{}:{}", direction.as_str(), result.as_str()));
    }

    pub fn record_routing_decision(&self, kind: RoutingDecisionKind) {
        self.routing_decisions.inc(kind.as_str());
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

    /// Set the worker-health gauges from a fleet count.
    pub fn set_worker_health_counts(&self, alive: u64, draining: u64, not_ready: u64, unknown: u64, dead: u64) {
        self.health.alive.store(alive, Ordering::Relaxed);
        self.health.draining.store(draining, Ordering::Relaxed);
        self.health.not_ready.store(not_ready, Ordering::Relaxed);
        self.health.unknown.store(unknown, Ordering::Relaxed);
        self.health.dead.store(dead, Ordering::Relaxed);
    }

    // --- read helpers (tests) ---
    pub fn messages_total(&self) -> u64 {
        self.messages.sum()
    }
    pub fn routing_decisions_total(&self) -> u64 {
        self.routing_decisions.sum()
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

        labeled(&mut s, "sip_messages_total", "SIP messages by direction+result.", "counter", "label", &self.messages.snapshot());
        labeled(&mut s, "sip_routing_decision_total", "Routing decisions by kind.", "counter", "kind", &self.routing_decisions.snapshot());
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
    fn worker_health_gauges_render() {
        let m = ProxyMetrics::new();
        m.set_worker_health_counts(1, 0, 0, 0, 0);
        let txt = m.prometheus_text();
        assert!(txt.contains("sip_worker_health{health=\"alive\"} 1"));
        assert!(txt.contains("sip_worker_health{health=\"draining\"} 0"));
    }
}
