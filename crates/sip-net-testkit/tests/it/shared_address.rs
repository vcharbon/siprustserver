//! A shared address on the simulated fabric: several endpoints serve one
//! address, each sender's flow delivered to the member its picker names, as
//! an ECMP next hop spreads flows.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sip_net::{BindErrorReason, BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::{FlowPick, SimulatedSignalingNetwork};
use tokio::time::timeout;

const SHARED: &str = "192.0.2.10:5060";
const LEFT: &str = "198.51.100.1:5060";
const RIGHT: &str = "198.51.100.2:5060";

fn addr(at: &str) -> SocketAddr {
    at.parse().unwrap()
}

fn opts(at: &str) -> BindUdpOpts {
    BindUdpOpts::new(addr(at), 64)
}

/// A picker reading a flow table the test edits; an unlisted sender goes to
/// member 0.
fn table(flows: &Arc<Mutex<HashMap<SocketAddr, usize>>>) -> FlowPick {
    let flows = flows.clone();
    Arc::new(move |src| flows.lock().unwrap().get(&src).copied().unwrap_or(0))
}

async fn recv(ep: &dyn UdpEndpoint) -> (Vec<u8>, SocketAddr) {
    let pkt = timeout(Duration::from_secs(1), ep.recv()).await.expect("recv timed out").unwrap();
    (pkt.raw.to_vec(), pkt.src)
}

#[tokio::test(start_paused = true)]
async fn each_senders_flow_reaches_the_member_its_picker_names() {
    let net = SimulatedSignalingNetwork::new(1);
    let flows = Arc::new(Mutex::new(HashMap::from([(addr(LEFT), 0), (addr(RIGHT), 1)])));
    net.clone().share_address(addr(SHARED), table(&flows));
    let first = net.bind_udp(opts(SHARED)).await.expect("the first member binds");
    let second = net.bind_udp(opts(SHARED)).await.expect("a second member joins");
    let left = net.bind_udp(opts(LEFT)).await.unwrap();
    let right = net.bind_udp(opts(RIGHT)).await.unwrap();

    left.send_to(b"from-left", addr(SHARED)).await.unwrap();
    right.send_to(b"from-right", addr(SHARED)).await.unwrap();
    assert_eq!(recv(first.as_ref()).await, (b"from-left".to_vec(), addr(LEFT)));
    assert_eq!(recv(second.as_ref()).await, (b"from-right".to_vec(), addr(RIGHT)));

    flows.lock().unwrap().insert(addr(LEFT), 1);
    left.send_to(b"moved", addr(SHARED)).await.unwrap();
    assert_eq!(recv(second.as_ref()).await, (b"moved".to_vec(), addr(LEFT)));
    assert!(first.try_recv().is_none(), "a moved flow no longer reaches its old member");
}

#[tokio::test(start_paused = true)]
async fn a_member_sends_from_the_shared_address() {
    let net = SimulatedSignalingNetwork::new(1);
    let flows = Arc::new(Mutex::new(HashMap::new()));
    net.share_address(addr(SHARED), table(&flows));
    let _first = net.bind_udp(opts(SHARED)).await.unwrap();
    let second = net.bind_udp(opts(SHARED)).await.unwrap();
    let left = net.bind_udp(opts(LEFT)).await.unwrap();

    second.send_to(b"out", addr(LEFT)).await.unwrap();
    assert_eq!(recv(left.as_ref()).await, (b"out".to_vec(), addr(SHARED)));
    assert_eq!(second.local_addr(), addr(SHARED));
}

#[tokio::test(start_paused = true)]
async fn dropping_a_member_frees_only_its_slot() {
    let net = SimulatedSignalingNetwork::new(1);
    let flows = Arc::new(Mutex::new(HashMap::from([(addr(LEFT), 0), (addr(RIGHT), 1)])));
    net.share_address(addr(SHARED), table(&flows));
    let first = net.bind_udp(opts(SHARED)).await.unwrap();
    let second = net.bind_udp(opts(SHARED)).await.unwrap();
    let left = net.bind_udp(opts(LEFT)).await.unwrap();
    let right = net.bind_udp(opts(RIGHT)).await.unwrap();
    drop(first);

    left.send_to(b"to-the-gone", addr(SHARED)).await.unwrap();
    right.send_to(b"to-the-kept", addr(SHARED)).await.unwrap();
    assert_eq!(recv(second.as_ref()).await, (b"to-the-kept".to_vec(), addr(RIGHT)));
    net.await_in_flight(Duration::from_secs(1)).await;
    let undelivered = net.drain_undeliverable().await;
    assert_eq!(undelivered.len(), 1, "the gone member's flow is undeliverable");
    assert_eq!(undelivered[0].src, addr(LEFT));

    let back = net.bind_udp(opts(SHARED)).await.expect("a new member takes the freed slot");
    left.send_to(b"returned", addr(SHARED)).await.unwrap();
    assert_eq!(recv(back.as_ref()).await, (b"returned".to_vec(), addr(LEFT)));
}

#[tokio::test(start_paused = true)]
async fn an_unshared_address_still_binds_once() {
    let net = SimulatedSignalingNetwork::new(1);
    net.share_address(addr(SHARED), Arc::new(|_| 0));
    let _member = net.bind_udp(opts(SHARED)).await.unwrap();
    let _alone = net.bind_udp(opts(LEFT)).await.unwrap();
    let err = net.bind_udp(opts(LEFT)).await.err().expect("expected AlreadyBound");
    assert_eq!(err.reason, BindErrorReason::AlreadyBound);
}
