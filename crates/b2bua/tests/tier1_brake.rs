//! Tier-1 overload brake, wired through the real `PreIngressHook` seam.
//!
//! The production hook ([`b2bua::tier1_brake::build_tier1_brake_hook`]) is
//! installed on one `SimulatedSignalingNetwork` bind (the "B2BUA" socket) with a
//! raw flooder bound on the same fabric. The B2BUA's ingress queue is never
//! drained, so it fills past the Tier-1 threshold and the brake starts refusing
//! new calls back to the flooder. The unit-level classification table lives in
//! `b2bua::tier1_brake::tests`; this file proves the wiring — the reject really
//! reaches the peer as a 503, and only the intended class of traffic is shed.
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
use std::time::Duration;

use b2bua::tier1_brake::{build_tier1_brake_hook, Tier1BrakeConfig, Tier1BrakeCounters};
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

/// Threshold 2, jitter 0 — so the reject's `Retry-After` is exactly the base.
fn brake_config() -> Tier1BrakeConfig {
    Tier1BrakeConfig {
        queue_max: QUEUE_MAX,
        tier1_threshold_pct: TIER1_PCT,
        retry_after_base_sec: 5,
        retry_after_jitter_sec: 0,
    }
}

/// An INVITE. `to_tag` makes it in-dialog (a re-INVITE); `emergency` adds the
/// `Resource-Priority: esnet.0` the brake bypasses on.
fn invite_buf(i: u32, emergency: bool, to_tag: Option<&str>) -> Vec<u8> {
    let to_param = to_tag.map(|t| format!(";tag={t}")).unwrap_or_default();
    let mut s = format!(
        "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-brake-{i}\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag-{i}\r\n\
To: <sip:bob@b2bua.test>{to_param}\r\n\
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

/// A new, non-emergency INVITE — the only class the brake refuses.
fn new_invite(i: u32) -> Vec<u8> {
    invite_buf(i, false, None)
}

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
    let hook = build_tier1_brake_hook(brake_config(), counters.clone(), &IdGen::seeded(11));

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

/// New non-emergency INVITEs past the threshold come back as the shared
/// reject-new-call 503 — overload `Reason`, base `Retry-After`, and a To-tag.
#[tokio::test(start_paused = true)]
async fn new_non_emergency_invites_past_the_threshold_are_rejected() {
    let (net, b2bua, flooder, counters) = setup().await;

    // Flood 10 INVITEs. Each send forks a TRANSIT_MS-delayed delivery into the
    // fabric; nothing drains the B2BUA ingress queue, so once depth crosses the
    // threshold (2) every subsequent INVITE takes the reply (503) path.
    let flood = 10u32;
    for i in 0..flood {
        flooder.send_to(&new_invite(i), b2bua_addr()).await.expect("flooder send");
    }

    // Hop 1: all 10 arrival forks fire (depths 0,1 accept; 2+ reply 503). Hop 2:
    // the 503 reply deliveries land back at the flooder. `settle` drives both.
    settle(&net).await;

    let expected_rejects = (flood - 2) as u64;
    assert_eq!(counters.drops_tier1_brake(), expected_rejects);
    assert_eq!(counters.tier1_reject_sent(), expected_rejects);
    // The B2BUA enqueued exactly the two below-threshold INVITEs.
    assert_eq!(b2bua.queue_depth(), 2);
    // And the endpoint's generic pre-ingress-reply counter agrees with the
    // brake's own (the fabric bumps `pre_ingress_replies` on every Reply action).
    assert_eq!(b2bua.counters().pre_ingress_replies, expected_rejects);

    let replies = drain(flooder.as_ref());
    assert_eq!(
        replies.len(),
        expected_rejects as usize,
        "flooder must receive exactly the brake's rejects and no more"
    );
    for raw in &replies {
        assert_eq!(status_line(raw), b"SIP/2.0 503 Service Unavailable");
        // jitter==0 → Retry-After is exactly the base (5).
        assert!(
            find(raw, b"Retry-After: 5\r\n").is_some(),
            "reject must carry the base Retry-After; got {:?}",
            String::from_utf8_lossy(raw)
        );
        assert!(
            find(raw, b"Reason: SIP;cause=503;text=\"overload\"\r\n").is_some(),
            "reject must carry the overload cause; got {:?}",
            String::from_utf8_lossy(raw)
        );
        let text = String::from_utf8(raw.clone()).expect("utf-8 reject");
        let to_line = text.lines().find(|l| l.starts_with("To:")).expect("a To line");
        assert!(to_line.contains(";tag="), "reject must tag To: {to_line}");
    }
}

/// Emergency INVITEs are admitted even above the threshold, and the bypass is
/// counted.
#[tokio::test(start_paused = true)]
async fn emergency_invites_bypass_the_brake_even_above_the_threshold() {
    let (net, b2bua, flooder, counters) = setup().await;

    // Two non-emergency INVITEs (accepted, fill up to threshold), then one
    // emergency INVITE that would otherwise trip the brake.
    flooder.send_to(&new_invite(0), b2bua_addr()).await.unwrap();
    flooder.send_to(&new_invite(1), b2bua_addr()).await.unwrap();
    flooder.send_to(&invite_buf(2, true, None), b2bua_addr()).await.unwrap();
    settle(&net).await;

    // All three enqueued; no reject sent.
    assert_eq!(counters.drops_tier1_brake(), 0);
    assert_eq!(counters.tier1_reject_sent(), 0);
    assert_eq!(counters.emergency_bypassed(), 1);
    assert_eq!(b2bua.queue_depth(), 3);
    assert!(flooder.try_recv().is_none(), "emergency INVITE must not be rejected");
}

/// A re-INVITE names an existing dialog by its To-tag: the brake refuses NEW
/// calls only, so an established call is never disturbed by ingress overload.
#[tokio::test(start_paused = true)]
async fn in_dialog_reinvites_are_never_braked() {
    let (net, b2bua, flooder, counters) = setup().await;

    // Saturate with new INVITEs (rejected past the threshold), then send a
    // re-INVITE for an established dialog at the same saturated depth.
    for i in 0..3u32 {
        flooder.send_to(&new_invite(i), b2bua_addr()).await.unwrap();
    }
    flooder
        .send_to(&invite_buf(7, false, Some("bob-tag-7")), b2bua_addr())
        .await
        .unwrap();
    settle(&net).await;

    // Only the new INVITEs above the threshold were refused (indexes 2..).
    assert_eq!(counters.tier1_reject_sent(), 1);
    // The re-INVITE was ADMITTED, not merely un-rejected: the two
    // below-threshold INVITEs plus the re-INVITE are on the ingress queue.
    assert_eq!(b2bua.queue_depth(), 3, "the re-INVITE must be enqueued for the pipeline");
    let replies = drain(flooder.as_ref());
    assert_eq!(replies.len(), 1, "the re-INVITE draws no reject");
    assert_eq!(status_line(&replies[0]), b"SIP/2.0 503 Service Unavailable");
}

/// Non-INVITE requests are outside the brake's goal — they are admitted at any
/// depth.
#[tokio::test(start_paused = true)]
async fn non_invite_requests_are_not_rejected_by_the_brake() {
    let (net, b2bua, flooder, counters) = setup().await;

    // Saturate with INVITEs so the queue is at/above threshold, then fire an
    // OPTIONS — accepted, since the brake refuses new calls only.
    for i in 0..5u32 {
        flooder.send_to(&new_invite(i), b2bua_addr()).await.unwrap();
    }
    flooder.send_to(&options_buf(0), b2bua_addr()).await.unwrap();
    settle(&net).await;

    // Brake refused 3 of the 5 INVITEs (depth >= 2 for indexes 2..4).
    assert_eq!(counters.tier1_reject_sent(), 3);
    // The OPTIONS was ADMITTED, not merely un-rejected: it sits on the ingress
    // queue behind the two below-threshold INVITEs.
    assert_eq!(b2bua.queue_depth(), 3, "the OPTIONS must be enqueued for the pipeline");

    let replies = drain(flooder.as_ref());
    assert_eq!(replies.len(), 3, "exactly the 3 INVITE rejects; the OPTIONS draws none");
    for raw in &replies {
        assert_eq!(status_line(raw), b"SIP/2.0 503 Service Unavailable");
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
