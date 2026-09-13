//! `UdpTransportMetrics` shape — end-to-end through the simulated fabric.
//!
//! The brake's *decision* half (which INVITEs are rejected, emergency bypass,
//! non-INVITE and in-dialog pass-through) is covered in `tests/tier1_brake.rs`.
//! THIS file proves the metrics shape reports the LIVE values: brake drops /
//! rejects sent from the hook's counters, and queue depth / tail-drops read
//! through the bound endpoint.
//!
//! Setup mirrors the brake test: one B2BUA bind (production brake hook
//! installed, its counters folded into a `UdpTransportMetrics`, NEVER drained so
//! the queue fills) + one raw flooder on the same fabric. We flood past the
//! Tier-1 threshold AND past `queue_max` so the shape reports a real queue depth
//! AND a non-zero tail-drop count.
//!
//! Clock: `#[tokio::test(start_paused = true)]` (CLAUDE.md) — same spawn-per-
//! datagram quiescence discipline as `tier1_brake.rs` (advance one transit hop,
//! yield until `in_flight == 0`). No real wall-clock cost — default lane.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::tier1_brake::{build_tier1_brake_hook, Tier1BrakeConfig, Tier1BrakeCounters};
use b2bua::UdpTransportMetrics;
use sip_net::types::BindUdpOpts;
use sip_net::{SignalingNetwork, SimulatedSignalingNetwork, UdpEndpoint};
use sip_txn::IdGen;

const TRANSIT_MS: u64 = 15;
const QUEUE_MAX: usize = 5;
// Tier-1 threshold = floor(QUEUE_MAX * TIER1_PCT / 100) = floor(5 * 40 / 100) = 2.
const TIER1_PCT: u32 = 40;
const B2BUA_ADDR: &str = "127.0.0.1:5060";
const FLOODER_ADDR: &str = "10.0.0.1:5555";

fn b2bua_addr() -> SocketAddr {
    B2BUA_ADDR.parse().unwrap()
}
fn flooder_addr() -> SocketAddr {
    FLOODER_ADDR.parse().unwrap()
}

fn brake_config() -> Tier1BrakeConfig {
    Tier1BrakeConfig {
        queue_max: QUEUE_MAX,
        tier1_threshold_pct: TIER1_PCT,
        retry_after_base_sec: 5,
        retry_after_jitter_sec: 0,
    }
}

/// A new (To-tag-less) INVITE. With `emergency`, carries the
/// `Resource-Priority: esnet.0` the brake bypasses on.
fn invite_buf(i: u32, emergency: bool) -> Vec<u8> {
    let mut s = format!(
        "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-brake-{i}\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag-{i}\r\n\
To: <sip:bob@b2bua.test>\r\n\
Call-ID: brake-test-{i}@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5555>\r\n\
Max-Forwards: 70\r\n"
    );
    if emergency {
        s.push_str("Resource-Priority: esnet.0\r\n");
    }
    s.push_str("Content-Length: 0\r\n\r\n");
    s.into_bytes()
}

/// Bind the B2BUA side (brake hook installed; counters folded into a live
/// `UdpTransportMetrics` whose `queueDepth`/`dropsTailDrop` read THROUGH the
/// bound endpoint) and a raw flooder on the same fabric. Returns the fabric, the
/// (never-drained) B2BUA endpoint, the flooder, and the metrics shape.
async fn setup(
) -> (SimulatedSignalingNetwork, Arc<dyn UdpEndpoint>, Box<dyn UdpEndpoint>, UdpTransportMetrics) {
    let net = SimulatedSignalingNetwork::new(TRANSIT_MS);
    let brake = Tier1BrakeCounters::new();
    let hook = build_tier1_brake_hook(brake_config(), brake.clone(), &IdGen::seeded(3));

    let b2bua: Arc<dyn UdpEndpoint> = net
        .bind_udp(BindUdpOpts::new(b2bua_addr(), QUEUE_MAX).with_pre_ingress(hook))
        .await
        .expect("bind b2bua")
        .into();
    let flooder = net.bind_udp(BindUdpOpts::new(flooder_addr(), 64)).await.expect("bind flooder");

    // Live getters over the bound endpoint — the TS `get queueDepth()` /
    // `get dropsTailDrop()`. A clone of the Arc'd endpoint is captured by each
    // closure so the shape reads the instantaneous value on every access.
    let ep_depth = b2bua.clone();
    let ep_tail = b2bua.clone();
    let ep_would_block = b2bua.clone();
    let metrics = UdpTransportMetrics::new(
        QUEUE_MAX,
        brake,
        Arc::new(move || ep_depth.queue_depth() as u64),
        Arc::new(move || ep_tail.counters().tail_dropped),
        Arc::new(move || ep_would_block.counters().send_would_block),
    );

    (net, b2bua, flooder, metrics)
}

/// Drive the paused clock until the simulated fabric is quiescent (same shape as
/// `tier1_brake.rs::settle`).
async fn settle(net: &SimulatedSignalingNetwork) {
    for _ in 0..64 {
        tokio::time::advance(Duration::from_millis(TRANSIT_MS)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        if net.in_flight() == 0 {
            return;
        }
    }
    panic!("simulated fabric never settled; in_flight={}", net.in_flight());
}

/// Flood 10 INVITEs into an undrained queue, read through the
/// `UdpTransportMetrics` shape: the
/// first two (depth 0,1) are accepted and enqueued; depth then sits at the
/// threshold (2) and every later non-emergency INVITE is shed. The metrics shape
/// reports `drops == rejects_sent == flood - 2` and `queue_depth == 2`, via the
/// live getters.
#[tokio::test(start_paused = true)]
async fn metrics_shape_reports_brake_drops_and_queue_depth() {
    let (net, _b2bua, flooder, metrics) = setup().await;

    let flood = 10u32;
    for i in 0..flood {
        flooder.send_to(&invite_buf(i, false), b2bua_addr()).await.expect("flooder send");
    }
    settle(&net).await;

    let expected_rejects = (flood - 2) as u64;
    assert_eq!(metrics.drops_tier1_brake(), expected_rejects, "brake drops");
    assert_eq!(metrics.tier1_reject_sent(), expected_rejects, "brake rejects sent");
    // queueDepth is the live endpoint depth — the two below-threshold INVITEs.
    assert_eq!(metrics.queue_depth(), 2, "live queue depth");
    assert_eq!(metrics.queue_max(), QUEUE_MAX);
    // Below threshold + at capacity (5): the brake shed everything over the
    // threshold (2) before the queue could fill, so nothing tail-dropped.
    assert_eq!(metrics.drops_tail_drop(), 0, "no tail-drop: the brake shed first");

    // The render carries the live values.
    let txt = metrics.prometheus_text();
    assert!(txt.contains(&format!("b2bua_udp_tier1_brake_drops_total {expected_rejects}")));
    assert!(txt.contains("b2bua_udp_queue_depth 2"));
    assert!(txt.contains("b2bua_udp_queue_max 5"));
    assert!(txt.contains("b2bua_udp_tail_dropped_total 0"));
    assert!(txt.contains("b2bua_udp_send_would_block_total 0"));
}

/// The tail-drop count is a LIVE proxy of `endpoint.counters.tail_dropped`. The brake
/// only sheds INVITEs; an OPTIONS flood past `queue_max` is accepted by the hook
/// but TAIL-DROPPED by the full bounded queue once depth hits the cap — and the
/// metrics shape surfaces that count (the blind spot a depth-only view misses).
#[tokio::test(start_paused = true)]
async fn metrics_shape_reports_live_tail_drop() {
    let (net, _b2bua, flooder, metrics) = setup().await;

    // Fire QUEUE_MAX + 4 OPTIONS (the brake passes non-INVITEs). The queue caps
    // at QUEUE_MAX (5); the 4 over capacity tail-drop.
    let n = (QUEUE_MAX + 4) as u32;
    for i in 0..n {
        let opts = format!(
            "OPTIONS sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-opts-{i}\r\n\
From: <sip:alice@flooder.test>;tag=opt-{i}\r\n\
To: <sip:bob@b2bua.test>\r\n\
Call-ID: opts-{i}@10.0.0.1\r\n\
CSeq: 1 OPTIONS\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\r\n"
        );
        flooder.send_to(opts.as_bytes(), b2bua_addr()).await.unwrap();
    }
    settle(&net).await;

    // No INVITE → the brake never fired.
    assert_eq!(metrics.drops_tier1_brake(), 0);
    assert_eq!(metrics.tier1_reject_sent(), 0);
    // Queue capped at QUEUE_MAX; the surplus tail-dropped (live counter).
    assert_eq!(metrics.queue_depth(), QUEUE_MAX as u64, "queue filled to cap");
    assert_eq!(metrics.drops_tail_drop(), (n as u64) - QUEUE_MAX as u64, "surplus tail-dropped");

    let txt = metrics.prometheus_text();
    assert!(txt.contains(&format!("b2bua_udp_queue_depth {QUEUE_MAX}")));
    assert!(txt.contains(&format!("b2bua_udp_tail_dropped_total {}", n as u64 - QUEUE_MAX as u64)));
}

/// Emergency INVITEs bypass the brake, so the shape's brake counters stay at
/// zero even above the threshold and the depth reflects all three enqueued.
#[tokio::test(start_paused = true)]
async fn metrics_shape_emergency_bypass_keeps_brake_counters_zero() {
    let (net, _b2bua, flooder, metrics) = setup().await;

    flooder.send_to(&invite_buf(0, false), b2bua_addr()).await.unwrap();
    flooder.send_to(&invite_buf(1, false), b2bua_addr()).await.unwrap();
    flooder.send_to(&invite_buf(2, true), b2bua_addr()).await.unwrap();
    settle(&net).await;

    assert_eq!(metrics.drops_tier1_brake(), 0, "brake drops");
    assert_eq!(metrics.tier1_reject_sent(), 0, "brake rejects sent");
    assert_eq!(metrics.queue_depth(), 3, "live queue depth — all three enqueued");
    assert_eq!(metrics.drops_tail_drop(), 0);
}
