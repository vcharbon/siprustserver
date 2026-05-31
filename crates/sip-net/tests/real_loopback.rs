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
