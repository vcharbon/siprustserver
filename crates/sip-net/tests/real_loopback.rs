//! Real impl over loopback UDP — the behaviours TestClock can't model (real
//! sockets), the source's `it.live` tier. Each test binds ephemeral
//! `127.0.0.1:0` sockets and talks between them.

use std::sync::Arc;
use std::time::Duration;

use sip_net::types::PreIngressAction;
use sip_net::{BindUdpOpts, PreIngressHook, RealSignalingNetwork, SignalingNetwork};
use tokio::time::timeout;

fn loopback(queue_max: usize) -> BindUdpOpts {
    BindUdpOpts::new("127.0.0.1:0".parse().unwrap(), queue_max)
}

#[tokio::test]
async fn loopback_send_recv() {
    let net = RealSignalingNetwork::new();
    let a = net.bind_udp(loopback(64)).await.unwrap();
    let b = net.bind_udp(loopback(64)).await.unwrap();

    a.send_to(b"hello over the wire", b.local_addr())
        .await
        .unwrap();

    let pkt = timeout(Duration::from_secs(2), b.recv())
        .await
        .expect("recv timed out")
        .expect("queue closed");
    assert_eq!(pkt.raw, b"hello over the wire");
    assert_eq!(pkt.src.ip(), a.local_addr().ip());
    assert_eq!(b.counters().enqueued, 1);
}

#[tokio::test]
async fn pre_ingress_reply_round_trips() {
    let net = RealSignalingNetwork::new();
    let hook: PreIngressHook = Arc::new(|raw: &[u8], _src, _depth| {
        if raw == b"PING" {
            PreIngressAction::Reply(b"PONG".to_vec())
        } else {
            PreIngressAction::Accept
        }
    });
    let a = net.bind_udp(loopback(64)).await.unwrap();
    let b = net.bind_udp(loopback(64).with_pre_ingress(hook)).await.unwrap();

    a.send_to(b"PING", b.local_addr()).await.unwrap();

    let reply = timeout(Duration::from_secs(2), a.recv())
        .await
        .expect("reply timed out")
        .unwrap();
    assert_eq!(reply.raw, b"PONG");
    assert_eq!(b.counters().pre_ingress_replies, 1);
    assert_eq!(b.counters().enqueued, 0);
}

#[tokio::test]
async fn real_has_no_transit_or_inflight() {
    let net = RealSignalingNetwork::new();
    assert_eq!(net.transit_delay_ms(), None);
    assert_eq!(net.in_flight(), 0);
    assert!(net.queue_depths().is_empty());
    assert!(net.drain_undeliverable().await.is_empty());
}

/// SO_REUSEPORT sharding (Pass 9): two endpoints bind the SAME addr:port, and
/// every datagram of one flow (one src socket) lands on exactly ONE of them —
/// the kernel 4-tuple flow-hash that preserves per-flow ordering when the
/// proxy shards its recv loop.
#[tokio::test]
async fn reuse_port_shards_one_flow_to_one_socket() {
    let net = RealSignalingNetwork::new();
    // First bind picks the port (reuse_port set so the second can join it).
    let s1 = net
        .bind_udp(loopback(64).with_reuse_port(true))
        .await
        .expect("first reuse-port bind");
    let addr = s1.local_addr();
    let s2 = net
        .bind_udp(BindUdpOpts::new(addr, 64).with_reuse_port(true))
        .await
        .expect("second reuse-port bind on the same port");
    assert_eq!(s2.local_addr(), addr);

    // One flow: a single source socket sends N datagrams to the shared port.
    let uac = net.bind_udp(loopback(64)).await.unwrap();
    const N: usize = 20;
    for i in 0..N {
        uac.send_to(format!("pkt-{i}").as_bytes(), addr).await.unwrap();
    }

    // All N land on exactly one shard, in order. Wait until the kernel has
    // delivered all N (loopback — fast), then drain via try_recv.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while s1.counters().enqueued + s2.counters().enqueued < N as u64 {
        assert!(tokio::time::Instant::now() < deadline, "flow never fully arrived");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (e1, e2) = (s1.counters().enqueued, s2.counters().enqueued);
    assert!(
        (e1 == N as u64 && e2 == 0) || (e2 == N as u64 && e1 == 0),
        "one flow must hash to exactly one shard (got s1={e1}, s2={e2})",
    );
    let receiver = if e1 > 0 { &s1 } else { &s2 };
    let got: Vec<usize> = std::iter::from_fn(|| receiver.try_recv())
        .map(|pkt| std::str::from_utf8(&pkt.raw).unwrap().strip_prefix("pkt-").unwrap().parse().unwrap())
        .collect();
    assert_eq!(got, (0..N).collect::<Vec<_>>(), "per-flow order preserved on one shard");
}

/// Without reuse_port, a second bind on a taken port still fails loudly.
#[tokio::test]
async fn plain_rebind_still_conflicts() {
    let net = RealSignalingNetwork::new();
    let s1 = net.bind_udp(loopback(64)).await.unwrap();
    let err = net.bind_udp(BindUdpOpts::new(s1.local_addr(), 64)).await;
    assert!(err.is_err(), "non-reuse-port rebind must fail");
}
