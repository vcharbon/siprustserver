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
    assert!(txt
        .contains("sip_proxy_overload_rejections_total{reason=\"no_target_critical_filtered\"} 1"));
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
    assert!(txt.contains(&format!("sip_proxy_requests_total{{method=\"{OVERFLOW}\"}} {past}\n")));
    assert!(txt.contains(&format!("\nsip_proxy_requests_overflow_total {past}\n")));
    assert_eq!(series("sip_proxy_responses_total{method=\"BAR"), DEFAULT_CAP);
    assert!(txt.contains(&format!("\nsip_proxy_responses_overflow_total {past}\n")));
    assert!(txt.contains("sip_proxy_requests_total{method=\"FOO0\"} 1\n"), "{txt}");
    assert!(!txt.contains("FOO999"), "a token past the cap gets no series of its own");
}
