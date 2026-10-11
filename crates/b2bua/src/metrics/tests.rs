use super::*;

#[test]
fn per_method_request_response_render() {
    let m = B2buaMetrics::new();
    m.record_request("invite");
    m.record_request("BYE");
    m.record_response("invite", 200);
    m.record_response("BYE", 200);
    let txt = m.prometheus_text();
    assert!(txt.contains("b2bua_requests_total{method=\"INVITE\"} 1"));
    assert!(txt.contains("b2bua_requests_total{method=\"BYE\"} 1"));
    assert!(txt.contains("b2bua_responses_total{method=\"INVITE\",code=\"200\"} 1"));
    assert!(txt.contains("b2bua_responses_total{method=\"BYE\",code=\"200\"} 1"));
}

#[test]
fn unroutable_series_are_published_at_zero_from_startup() {
    let txt = B2buaMetrics::new().prometheus_text();
    assert!(txt.contains("b2bua_unroutable_dropped_total{kind=\"ACK\"} 0"));
    assert!(txt.contains("b2bua_unroutable_refused_total{method=\"BYE\",code=\"481\"} 0"));
    assert!(txt.contains("b2bua_unroutable_internal_total{event=\"timeout\",method=\"OPTIONS\"} 0"));
}

#[test]
fn unroutable_classes_render_apart() {
    let m = B2buaMetrics::new();
    m.record_unroutable_dropped("ACK");
    m.record_unroutable_dropped("2xx");
    m.record_unroutable_refused("BYE", 481);
    m.record_unroutable_internal("timeout", "OPTIONS");
    assert_eq!(m.unroutable_dropped_total(), 2, "only wire messages left unanswered");
    let txt = m.prometheus_text();
    assert!(txt.contains("b2bua_unroutable_dropped_total{kind=\"ACK\"} 1"));
    assert!(txt.contains("b2bua_unroutable_dropped_total{kind=\"2xx\"} 1"));
    assert!(txt.contains("b2bua_unroutable_refused_total{method=\"BYE\",code=\"481\"} 1"));
    assert!(txt.contains("b2bua_unroutable_internal_total{event=\"timeout\",method=\"OPTIONS\"} 1"));
}

#[test]
fn drain_exits_and_duration_render() {
    use crate::drain::{DrainExit, DrainOutcome};
    use crate::limiter::release_queue::ReleaseFlush;
    let m = B2buaMetrics::new();
    let flush =
        ReleaseFlush { queued: 2, given_up: 0, elapsed: std::time::Duration::from_millis(300) };
    m.record_drain_exit(&DrainOutcome {
        exit: DrainExit::CaughtUp,
        residual: 1,
        elapsed: std::time::Duration::from_millis(1_200),
        release_flush: flush,
    });
    m.record_drain_exit(&DrainOutcome {
        exit: DrainExit::GracePeersBehind,
        residual: 1,
        elapsed: std::time::Duration::from_secs(5),
        release_flush: ReleaseFlush::default(),
    });
    assert_eq!(
        m.limiter().release_flushes_total(crate::limiter::release_queue::ReleaseFlushOutcome::Sent),
        1
    );
    assert_eq!(
        m.limiter()
            .release_flushes_total(crate::limiter::release_queue::ReleaseFlushOutcome::Empty),
        1
    );
    assert_eq!(m.drain_exits("caught_up"), 1);
    assert_eq!(m.drain_exits("grace_peers_behind"), 1);
    assert_eq!(m.drain_exits("quiescent"), 0);
    let txt = m.prometheus_text();
    assert!(txt.contains("b2bua_drain_exits_total{reason=\"caught_up\"} 1"));
    assert!(txt.contains("b2bua_drain_exits_total{reason=\"grace_peers_behind\"} 1"));
    // 1.2 s falls in every bucket from 2 up; 5 s in the 5 and 10 buckets.
    assert!(txt.contains("b2bua_drain_seconds_bucket{le=\"1\"} 0"));
    assert!(txt.contains("b2bua_drain_seconds_bucket{le=\"2\"} 1"));
    assert!(txt.contains("b2bua_drain_seconds_bucket{le=\"5\"} 2"));
    assert!(txt.contains("b2bua_drain_seconds_bucket{le=\"+Inf\"} 2"));
    assert!(txt.contains("b2bua_drain_seconds_sum 6.2"));
    assert!(txt.contains("b2bua_drain_seconds_count 2"));
    assert!(txt.contains("b2bua_limiter_release_flushes_total{outcome=\"sent\"} 1"));
    assert!(txt.contains("b2bua_limiter_release_flush_seconds_total 0.3"));
}

#[test]
fn retransmit_and_give_up_families_render() {
    let m = B2buaMetrics::new();
    m.record_retransmit("final-2xx", "invite", Some(200));
    m.record_retransmit("final-2xx", "INVITE", Some(200));
    m.record_retransmit("reliable-provisional", "INVITE", Some(183));
    m.record_retransmit("trigger", "ACK", None);
    m.record_repeat_give_up("ack-of-2xx");
    m.record_quiet_turn("own-rung");
    m.record_quiet_turn("own-rung");
    assert_eq!(m.repl_quiet_turns_total("own-rung"), 2);
    assert_eq!(m.repl_quiet_turns_total("re-ack"), 0);
    assert_eq!(m.retransmits_total("final-2xx", "INVITE", Some(200)), 2);
    assert_eq!(m.retransmits_total("trigger", "ACK", None), 1);
    assert_eq!(
        m.retransmits_total("trigger", "ACK", Some(200)),
        0,
        "a request row carries no code"
    );
    assert_eq!(m.repeat_give_ups_total("ack-of-2xx"), 1);
    assert_eq!(m.repeat_give_ups_total("await-prack"), 0);
    let txt = m.prometheus_text();
    assert!(
        txt.contains(
            "b2bua_retransmits_total{ladder=\"final-2xx\",method=\"INVITE\",code=\"200\"} 2"
        ),
        "{txt}"
    );
    assert!(txt.contains(
        "b2bua_retransmits_total{ladder=\"reliable-provisional\",method=\"INVITE\",code=\"183\"} 1"
    ));
    assert!(
        txt.contains("b2bua_retransmits_total{ladder=\"trigger\",method=\"ACK\"} 1"),
        "no code label on a request: {txt}"
    );
    assert!(txt.contains("b2bua_repeat_give_ups_total{obligation=\"ack-of-2xx\"} 1"));
    assert!(txt.contains("b2bua_repl_quiet_turns_total{kind=\"own-rung\"} 2"), "{txt}");
}

#[test]
fn clock_wall_divergence_gauge_round_trips_signed_and_renders() {
    let m = B2buaMetrics::new();
    // Unset → 0 (flat gauge).
    assert_eq!(m.clock_wall_divergence_ms(), 0);
    assert!(m.prometheus_text().contains("b2bua_clock_wall_divergence_ms 0"));
    // A forward host step (positive) round-trips.
    m.set_clock_wall_divergence_ms(300_000);
    assert_eq!(m.clock_wall_divergence_ms(), 300_000);
    assert!(m.prometheus_text().contains("b2bua_clock_wall_divergence_ms 300000"));
    // A backward step (negative) survives the u64-bit-pattern storage.
    m.set_clock_wall_divergence_ms(-45_000);
    assert_eq!(m.clock_wall_divergence_ms(), -45_000);
    assert!(m.prometheus_text().contains("b2bua_clock_wall_divergence_ms -45000"));
}

#[test]
fn memory_attribution_gauges_render() {
    let m = B2buaMetrics::new();
    // Unset → render at 0 (a flat gauge, not a missing series).
    let zero = m.prometheus_text();
    assert!(zero.contains("b2bua_store_calls 0"));
    assert!(zero.contains("b2bua_repl_meta_backup 0"));

    m.set_store_gauges(7, 11, 7, 9, 3, 7);
    m.set_repl_store_gauges(40, 22, 64, 4);
    let txt = m.prometheus_text();
    // A store_locks (9) > store_calls (7) gap is exactly the lock-leak signal.
    assert!(txt.contains("b2bua_store_calls 7"));
    assert!(txt.contains("b2bua_store_sip_index 11"));
    assert!(txt.contains("b2bua_store_indexed 7"));
    assert!(txt.contains("b2bua_store_locks 9"));
    assert!(txt.contains("b2bua_store_takeover_at 3"));
    assert!(txt.contains("b2bua_store_touched 7"));
    assert!(txt.contains("b2bua_repl_meta_total 40"));
    assert!(txt.contains("b2bua_repl_meta_backup 22"));
    assert!(txt.contains("b2bua_repl_changelog_entries 64"));
    assert!(txt.contains("b2bua_repl_changelog_peers 4"));
    assert!(txt.contains("b2bua_withdrawn_running 0"));
    m.set_withdrawn_running(true);
    let txt = m.prometheus_text();
    assert!(txt.contains("b2bua_withdrawn_running 1"));
    assert!(txt.contains("# TYPE b2bua_withdrawn_running gauge"));
    m.set_repl_peers_pulled_not_ready(1);
    let txt = m.prometheus_text();
    assert!(txt.contains("b2bua_repl_peers_pulled_not_ready 1"));
    assert!(txt.contains("# TYPE b2bua_repl_peers_pulled_not_ready gauge"));
    // Each gauge series must carry its TYPE line (Prometheus exposition).
    assert!(txt.contains("# TYPE b2bua_store_calls gauge"));
    assert!(txt.contains("# TYPE b2bua_repl_meta_backup gauge"));
}

#[test]
fn sm_cursor_census_renders_and_overwrites() {
    let m = B2buaMetrics::new();
    // Unset → the gauge family is declared but emits no series.
    let zero = m.prometheus_text();
    assert!(zero.contains("# TYPE b2bua_sm_cursors gauge"));
    assert!(!zero.contains("b2bua_sm_cursors{"));

    let mut census = BTreeMap::new();
    census.insert(("global-call".to_string(), "Active".to_string()), 5);
    census.insert(("transfer".to_string(), "CRinging".to_string()), 2);
    m.set_sm_cursor_census(census);
    let txt = m.prometheus_text();
    assert!(txt.contains("b2bua_sm_cursors{machine=\"global-call\",state=\"Active\"} 5"));
    assert!(txt.contains("b2bua_sm_cursors{machine=\"transfer\",state=\"CRinging\"} 2"));

    // A fresh census OVERWRITES: a cursor that drained to zero disappears
    // rather than sticking at its last value (gauge, not counter).
    let mut next = BTreeMap::new();
    next.insert(("global-call".to_string(), "Active".to_string()), 3);
    m.set_sm_cursor_census(next);
    let txt = m.prometheus_text();
    assert!(txt.contains("b2bua_sm_cursors{machine=\"global-call\",state=\"Active\"} 3"));
    assert!(!txt.contains("machine=\"transfer\""));
}

// -----------------------------------------------------------------------
// UdpTransportMetrics shape
// -----------------------------------------------------------------------

/// A `UdpTransportMetrics` whose `queueDepth` / `dropsTailDrop` are backed by
/// caller-held atomics (standing in for a live `UdpEndpoint`), with the real
/// brake counters, without standing up a fabric — the fabric-proxy facet is
/// covered end-to-end in `tests/it/udp_transport_metrics.rs`.
fn shape() -> (UdpTransportMetrics, IngressBrakeCounters, Arc<AtomicU64>, Arc<AtomicU64>) {
    let brake = IngressBrakeCounters::new();
    let depth = Arc::new(AtomicU64::new(0));
    let tail = Arc::new(AtomicU64::new(0));
    let d = depth.clone();
    let t = tail.clone();
    let m = UdpTransportMetrics::new(
        5, // queue_max — the brake test's QUEUE_MAX
        brake.clone(),
        Arc::new(move || d.load(Ordering::Relaxed)),
        Arc::new(move || t.load(Ordering::Relaxed)),
        Arc::new(|| 0),
        Arc::new(|| 7),
    );
    (m, brake, depth, tail)
}

/// Every facet is a LIVE getter: a value changed in the underlying source is
/// seen by the metrics shape on the next read, never a stale snapshot.
#[test]
fn udp_transport_metrics_facets_are_live() {
    let (m, brake, depth, tail) = shape();
    // All zero initially.
    assert_eq!(m.queue_depth(), 0);
    assert_eq!(m.queue_max(), 5);
    assert_eq!(m.ingress_brake_emergency_bypassed(), 0);
    assert_eq!(m.drops_tail_drop(), 0);

    // Mutate the sources — the shape reflects them live.
    depth.store(2, Ordering::Relaxed);
    tail.store(7, Ordering::Relaxed);
    brake.record_emergency_bypass();
    brake.record_emergency_bypass();
    brake.record_emergency_bypass();
    assert_eq!(m.queue_depth(), 2);
    assert_eq!(m.drops_tail_drop(), 7);
    assert_eq!(m.ingress_brake_emergency_bypassed(), 3);
}

/// The render carries every field of the shape, with the right Prometheus
/// TYPE per field (counters for the monotonic sheds/tail-drops/buffered,
/// gauges for the instantaneous depth/max/peers).
#[test]
fn udp_transport_metrics_render() {
    let (m, _brake, depth, tail) = shape();
    depth.store(3, Ordering::Relaxed);
    tail.store(11, Ordering::Relaxed);
    let txt = m.prometheus_text();

    // Emergency-bypass visibility series (renders even at 0).
    assert!(txt.contains("b2bua_udp_ingress_brake_emergency_bypassed_total 0"));
    assert!(txt.contains("# TYPE b2bua_udp_ingress_brake_emergency_bypassed_total counter"));
    // Live queue facets.
    assert!(txt.contains("b2bua_udp_queue_depth 3"));
    assert!(txt.contains("b2bua_udp_queue_max 5"));
    assert!(txt.contains("b2bua_udp_tail_dropped_total 11"));
    assert!(txt.contains("b2bua_udp_kernel_rx_dropped_total 7"));
    assert!(txt.contains("# TYPE b2bua_udp_kernel_rx_dropped_total counter"));
    // Prometheus TYPE lines: gauges vs counters.
    assert!(txt.contains("# TYPE b2bua_udp_queue_depth gauge"));
    assert!(txt.contains("# TYPE b2bua_udp_queue_max gauge"));
    assert!(txt.contains("# TYPE b2bua_udp_tail_dropped_total counter"));
}
