//! Tier-1 overload-brake coverage for the wired `preIngress` hook (port of
//! `tests/sip/UdpTransport-brake.test.ts`).
//!
//! Flavor: direct, end-to-end-through-the-fabric — the same flavor as the TS
//! test. Instead of driving the full B2BUA router, we install the production
//! brake hook ([`b2bua::tier1_brake::build_tier1_brake_hook`]) on one
//! `SimulatedSignalingNetwork` bind (the "B2BUA" socket) and bind one raw flooder
//! endpoint on the *same* fabric. We then simply never drain the B2BUA's ingress
//! queue, so it fills past the Tier-1 threshold and later INVITEs are
//! stateless-503'd back to the flooder.
//!
//! This is the missing other half of migration item 10: that item ported the
//! brake *helpers* and replayed the decision predicate as pure-helper tests
//! (`sip_message::message_helpers` tests), explicitly noting "the facade port
//! later only has to wire the (already-tested) pieces together." This file is
//! that wiring, exercised through the real `PreIngressHook` seam honoured by
//! `sip_net::simulated::deliver` and through the brake's own counters (the port
//! of `UdpTransportMetrics.dropsTier1Brake` / `tier1RejectSent`).
//!
//! Clock: `#[tokio::test(start_paused = true)]` (CLAUDE.md). The simulated fabric
//! delivers each datagram after one `transit_delay_ms` hop, and a `Reply` (the
//! 503) re-spawns a follow-up delivery that travels back after a second hop. A
//! single bulk `advance` fires the sleeps but does NOT run the freshly-spawned
//! delivery task bodies — the woken task needs a scheduler turn — so we
//! `advance` one hop then `yield_now` until the fabric is fully quiescent
//! (`in_flight == 0`). This is the CLAUDE.md "drive the protocol between
//! advances" discipline applied to the spawn-per-datagram fabric. No real
//! wall-clock cost — default lane.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::tier1_brake::{build_tier1_brake_hook, RollFn, Tier1BrakeConfig, Tier1BrakeCounters};
use sip_net::types::BindUdpOpts;
use sip_net::{SignalingNetwork, SimulatedSignalingNetwork, UdpEndpoint};

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

/// The brake test's config: queueMax=5, pct=40 (threshold 2), jitter 0 (so the
/// 503's Retry-After is exactly the base and the roll is never consulted —
/// `retryAfterJitterSec: 0` in the TS testConfig).
fn brake_config() -> Tier1BrakeConfig {
    Tier1BrakeConfig {
        queue_max: QUEUE_MAX,
        tier1_threshold_pct: TIER1_PCT,
        retry_after_base_sec: 5,
        retry_after_jitter_sec: 0,
    }
}

/// A roll that panics — proves jitter==0 never draws it.
fn never_roll() -> RollFn {
    Arc::new(|| panic!("jitter==0 must not draw the Retry-After roll"))
}

/// Build a minimal valid INVITE buffer (mirror of the TS `buildInviteBuffer`).
/// With `emergency`, add a `Resource-Priority: esnet.0` — the canonical marker
/// the brake bypasses on.
fn invite_buf(i: u32, emergency: bool) -> Vec<u8> {
    let mut s = format!(
        "INVITE sip:bob@{ip}:{port} SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-brake-{i}\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag-{i}\r\n\
To: <sip:bob@b2bua.test>\r\n\
Call-ID: brake-test-{i}@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5555>\r\n\
Max-Forwards: 70\r\n",
        ip = "127.0.0.1",
        port = 5060u16,
    );
    if emergency {
        s.push_str("Resource-Priority: esnet.0\r\n");
    }
    s.push_str("Content-Length: 0\r\n\r\n");
    s.into_bytes()
}

/// Mirror of the TS `buildOptionsBuffer`.
fn options_buf(i: u32) -> Vec<u8> {
    format!(
        "OPTIONS sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-opts-{i}\r\n\
From: <sip:alice@flooder.test>;tag=opt-{i}\r\n\
To: <sip:bob@b2bua.test>\r\n\
Call-ID: opts-{i}@10.0.0.1\r\n\
CSeq: 1 OPTIONS\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// First line (status line) of a buffer.
fn status_line(raw: &[u8]) -> &[u8] {
    match raw.windows(2).position(|w| w == b"\r\n") {
        Some(end) => &raw[..end],
        None => raw,
    }
}

/// Stand up the shared fabric, the B2BUA bind (brake hook installed, NEVER
/// drained) and a raw flooder endpoint on the same fabric. Returns the fabric,
/// the flooder endpoint, and the brake counters so the test can read them.
async fn setup() -> (
    SimulatedSignalingNetwork,
    Box<dyn UdpEndpoint>, // b2bua side — kept (and never drained) so the queue fills
    Box<dyn UdpEndpoint>, // flooder
    Tier1BrakeCounters,
) {
    let net = SimulatedSignalingNetwork::new(TRANSIT_MS);
    let counters = Tier1BrakeCounters::new();
    let hook = build_tier1_brake_hook(brake_config(), counters.clone(), never_roll());

    let b2bua = net
        .bind_udp(BindUdpOpts::new(b2bua_addr(), QUEUE_MAX).with_pre_ingress(hook))
        .await
        .expect("bind b2bua");
    let flooder = net
        .bind_udp(BindUdpOpts::new(flooder_addr(), 64))
        .await
        .expect("bind flooder");

    (net, b2bua, flooder, counters)
}

/// Drive the paused clock until the simulated fabric is fully quiescent. Each
/// iteration advances one transit hop then yields generously so every woken
/// `deliver` task body runs to completion; the `Reply` (503) branch re-spawns a
/// follow-up delivery, so we loop until `in_flight` drains to 0. Bounded so a
/// stuck fabric panics instead of hanging.
async fn settle(net: &SimulatedSignalingNetwork) {
    for _ in 0..64 {
        tokio::time::advance(Duration::from_millis(TRANSIT_MS)).await;
        // One `yield_now` advances exactly one task hop; the deliver pipeline is
        // shallow (send-task → deliver → reply-task → deliver), so a handful of
        // yields per hop drains it. Sized generously — extra yields are free.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        if net.in_flight() == 0 {
            return;
        }
    }
    panic!("simulated fabric never settled; in_flight={}", net.in_flight());
}

/// Drain all packets currently queued at an endpoint (the fabric is quiescent,
/// so a `try_recv` that returns `None` means truly empty).
fn drain(ep: &dyn UdpEndpoint) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(pkt) = ep.try_recv() {
        out.push(pkt.raw);
    }
    out
}

/// Port of "non-emergency INVITEs past the threshold receive a stateless 503".
#[tokio::test(start_paused = true)]
async fn non_emergency_invites_past_the_threshold_receive_a_stateless_503() {
    let (net, b2bua, flooder, counters) = setup().await;

    // Flood 10 INVITEs. Each send forks a TRANSIT_MS-delayed delivery into the
    // fabric; nothing drains the B2BUA ingress queue, so once depth crosses the
    // threshold (2) every subsequent INVITE takes the reply (503) path.
    let flood = 10u32;
    for i in 0..flood {
        flooder
            .send_to(&invite_buf(i, false), b2bua_addr())
            .await
            .expect("flooder send");
    }

    // Hop 1: all 10 arrival forks fire (depths 0,1 accept; 2+ reply 503). Hop 2:
    // the 503 reply deliveries land back at the flooder. `settle` drives both.
    settle(&net).await;

    // preIngress saw depth=0,1 for the first two (accepted) and depth=2 for every
    // packet after (>= threshold → reply 503).
    let expected_rejects = (flood - 2) as u64;
    assert_eq!(counters.drops_tier1_brake(), expected_rejects);
    assert_eq!(counters.tier1_reject_sent(), expected_rejects);
    // The B2BUA enqueued exactly the two below-threshold INVITEs.
    assert_eq!(b2bua.queue_depth(), 2);
    // And the endpoint's generic pre-ingress-reply counter agrees with the
    // brake's own (the fabric bumps `pre_ingress_replies` on every Reply action).
    assert_eq!(b2bua.counters().pre_ingress_replies, expected_rejects);

    // The flooder received exactly `expected_rejects` stateless 503s back.
    let replies = drain(flooder.as_ref());
    assert_eq!(
        replies.len(),
        expected_rejects as usize,
        "flooder must receive exactly the brake's 503s and no more"
    );
    for raw in &replies {
        assert_eq!(status_line(raw), b"SIP/2.0 503 Service Unavailable");
        // jitter==0 → Retry-After is exactly the base (5).
        assert!(
            find(raw, b"Retry-After: 5\r\n").is_some(),
            "503 must carry the base Retry-After; got {:?}",
            String::from_utf8_lossy(raw)
        );
    }
}

/// Port of "emergency INVITEs bypass the brake even when above the threshold".
#[tokio::test(start_paused = true)]
async fn emergency_invites_bypass_the_brake_even_above_the_threshold() {
    let (net, b2bua, flooder, counters) = setup().await;

    // Two non-emergency INVITEs (accepted, fill up to threshold), then one
    // emergency INVITE that would otherwise trip the brake.
    flooder.send_to(&invite_buf(0, false), b2bua_addr()).await.unwrap();
    flooder.send_to(&invite_buf(1, false), b2bua_addr()).await.unwrap();
    flooder
        .send_to(&invite_buf(2, true), b2bua_addr())
        .await
        .unwrap();
    settle(&net).await;

    // All three enqueued; no reject sent.
    assert_eq!(counters.drops_tier1_brake(), 0);
    assert_eq!(counters.tier1_reject_sent(), 0);
    assert_eq!(b2bua.queue_depth(), 3);
    // No 503 came back to the flooder.
    assert!(flooder.try_recv().is_none(), "emergency INVITE must not be 503'd");
}

/// Port of "non-INVITE requests are not 503'd by the brake".
#[tokio::test(start_paused = true)]
async fn non_invite_requests_are_not_503d_by_the_brake() {
    let (net, _b2bua, flooder, counters) = setup().await;

    // Saturate with INVITEs so the queue is at/above threshold, then fire an
    // OPTIONS — should accept (the brake only targets new INVITEs).
    for i in 0..5u32 {
        flooder.send_to(&invite_buf(i, false), b2bua_addr()).await.unwrap();
    }
    flooder.send_to(&options_buf(0), b2bua_addr()).await.unwrap();
    settle(&net).await;

    // Brake rejected 3 of the 5 INVITEs (depth >= 2 for indexes 2..4).
    assert_eq!(counters.tier1_reject_sent(), 3);

    // The flooder receives 3 x 503 (for the rejected INVITEs). The OPTIONS was
    // enqueued at the B2BUA — no extra reply was sent.
    let replies = drain(flooder.as_ref());
    assert_eq!(replies.len(), 3, "exactly the 3 INVITE 503s; the OPTIONS draws none");
    for raw in &replies {
        assert_eq!(status_line(raw), b"SIP/2.0 503 Service Unavailable");
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
