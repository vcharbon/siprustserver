//! Simulated in-memory fabric — routing, faults, bounded-queue tail-drop,
//! pre-ingress, undeliverable, quiescence. The fake-transport behaviours the
//! source's `SignalingNetwork.simulated.ts` provided to the test harness.

use std::sync::Arc;
use std::time::Duration;

use sip_net::types::PreIngressAction;
use sip_net::{
    BindErrorReason, BindUdpOpts, PreIngressHook, SignalingNetwork, SimulatedSignalingNetwork,
};
use tokio::time::timeout;

fn opts(addr: &str, queue_max: usize) -> BindUdpOpts {
    BindUdpOpts::new(addr.parse().unwrap(), queue_max)
}

#[tokio::test]
async fn routes_packet_to_bound_endpoint() {
    let net = SimulatedSignalingNetwork::new(0);
    let a = net.bind_udp(opts("10.0.0.1:5060", 64)).await.unwrap();
    let b = net.bind_udp(opts("10.0.0.2:5060", 64)).await.unwrap();

    a.send_to(b"INVITE sip:bob", b.local_addr()).await.unwrap();

    let pkt = timeout(Duration::from_secs(1), b.recv())
        .await
        .expect("recv timed out")
        .expect("queue closed");
    assert_eq!(pkt.raw, b"INVITE sip:bob");
    assert_eq!(pkt.src, a.local_addr());
    assert_eq!(b.counters().enqueued, 1);
}

#[tokio::test]
async fn double_bind_same_addr_is_already_bound() {
    let net = SimulatedSignalingNetwork::new(0);
    let _a = net.bind_udp(opts("10.0.0.1:5060", 64)).await.unwrap();
    // `Box<dyn UdpEndpoint>` isn't Debug, so take the error out via `.err()`.
    let err = net
        .bind_udp(opts("10.0.0.1:5060", 64))
        .await
        .err()
        .expect("expected AlreadyBound");
    assert_eq!(err.reason, BindErrorReason::AlreadyBound);
}

#[tokio::test]
async fn send_to_unbound_addr_is_undeliverable() {
    let net = SimulatedSignalingNetwork::new(0);
    let a = net.bind_udp(opts("10.0.0.1:5060", 64)).await.unwrap();

    a.send_to(b"lost", "10.9.9.9:5060".parse().unwrap())
        .await
        .unwrap();
    net.await_in_flight(Duration::from_secs(1)).await;

    let undelivered = net.drain_undeliverable().await;
    assert_eq!(undelivered.len(), 1);
    assert_eq!(undelivered[0].dst, "10.9.9.9:5060".parse().unwrap());
    // Draining empties the buffer.
    assert!(net.drain_undeliverable().await.is_empty());
}

#[tokio::test]
async fn send_fault_fails_the_send_synchronously() {
    let net = SimulatedSignalingNetwork::new(0)
        .with_send_fault(Arc::new(|_src, _dst| Some("simulated EHOSTUNREACH".to_string())));
    let a = net.bind_udp(opts("10.0.0.1:5060", 64)).await.unwrap();

    let err = a
        .send_to(b"x", "10.0.0.2:5060".parse().unwrap())
        .await
        .unwrap_err();
    assert!(err.message.contains("EHOSTUNREACH"));
}

#[tokio::test]
async fn full_queue_tail_drops() {
    let net = SimulatedSignalingNetwork::new(0);
    let a = net.bind_udp(opts("10.0.0.1:5060", 64)).await.unwrap();
    let b = net.bind_udp(opts("10.0.0.2:5060", 2)).await.unwrap();
    let dst = b.local_addr();

    // No consumer on b: first 2 enqueue, the rest tail-drop.
    for _ in 0..5 {
        a.send_to(b"pkt", dst).await.unwrap();
    }
    net.await_in_flight(Duration::from_secs(1)).await;

    let c = b.counters();
    assert_eq!(c.enqueued, 2, "queue_max=2 enqueues 2");
    assert_eq!(c.tail_dropped, 3, "remaining 3 tail-drop");
    assert_eq!(b.queue_depth(), 2);

    // queue_depths surfaces the live depth for the layer-close audit.
    let depths = net.queue_depths();
    assert!(depths.contains(&(dst, 2)));
}

#[tokio::test]
async fn pre_ingress_drop_and_reply() {
    let net = SimulatedSignalingNetwork::new(0);

    // b drops anything that starts with 'D', replies "PONG" to anything that
    // starts with 'P', accepts the rest.
    let hook: PreIngressHook = Arc::new(|raw: &[u8], _src, _depth| match raw.first() {
        Some(b'D') => PreIngressAction::Drop,
        Some(b'P') => PreIngressAction::Reply(b"PONG".to_vec()),
        _ => PreIngressAction::Accept,
    });
    let a = net.bind_udp(opts("10.0.0.1:5060", 64)).await.unwrap();
    let b = net
        .bind_udp(opts("10.0.0.2:5060", 64).with_pre_ingress(hook))
        .await
        .unwrap();
    let dst = b.local_addr();

    a.send_to(b"DROP_ME", dst).await.unwrap();
    a.send_to(b"PING", dst).await.unwrap();
    a.send_to(b"ACCEPT", dst).await.unwrap();
    net.await_in_flight(Duration::from_secs(1)).await;

    // a should have received the PONG reply.
    let reply = timeout(Duration::from_secs(1), a.recv())
        .await
        .expect("reply timed out")
        .unwrap();
    assert_eq!(reply.raw, b"PONG");

    // b enqueued only ACCEPT; dropped DROP_ME; replied to PING.
    let c = b.counters();
    assert_eq!(c.pre_ingress_dropped, 1);
    assert_eq!(c.pre_ingress_replies, 1);
    assert_eq!(c.enqueued, 1);
    let accepted = b.try_recv().unwrap();
    assert_eq!(accepted.raw, b"ACCEPT");
}

#[tokio::test]
async fn dropping_endpoint_unbinds_it() {
    let net = SimulatedSignalingNetwork::new(0);
    {
        let _a = net.bind_udp(opts("10.0.0.1:5060", 64)).await.unwrap();
        assert_eq!(net.queue_depths().len(), 1);
    }
    // After the endpoint drops, the routing slot is free again.
    assert_eq!(net.queue_depths().len(), 0);
    let _a2 = net.bind_udp(opts("10.0.0.1:5060", 64)).await.unwrap();
}
