//! Heap allocations of the mux per datagram, with every watcher on: the
//! retransmit engine, the late-answer hold, the round-trip tracker, the
//! message tally and the wire tap. One callee endpoint on the simulated
//! network under a paused clock, one thread, so the counts are exact.
//!
//! - A datagram the stores keep is copied once, whatever the number of stores
//!   keeping it.
//! - A datagram is sniffed once for every watcher: a re-sent final costs the
//!   mux its one shared copy, an absorbed duplicate costs the mux nothing.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use alloc_counter::{measure, AllocCost, CountingAlloc};
use loadgen::mux::CallTally;
use loadgen::outcome::CallWire;
use loadgen::{CallRouting, Correlation, EndpointSpec, MuxCore, Role};
use sip_clock::Clock;
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;
use tokio::runtime::Runtime;

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const RECV: Duration = Duration::from_secs(20);
const BOB: u16 = 47101;
const PEER: u16 = 47109;

fn addr(p: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], p))
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap()
}

/// The peer's initial INVITE to bob.
fn invite() -> Vec<u8> {
    b"INVITE sip:bob@127.0.0.1:47101 SIP/2.0\r\n\
      Via: SIP/2.0/UDP 127.0.0.1:47109;branch=z9hG4bK-inv1\r\nMax-Forwards: 70\r\n\
      Call-ID: cid-alloc\r\nX-Loadgen-Id: tok-alloc\r\n\
      From: <sip:peer@127.0.0.1>;tag=p1\r\nTo: <sip:bob@127.0.0.1>\r\nCSeq: 1 INVITE\r\n\
      Contact: <sip:peer@127.0.0.1:47109>\r\nContent-Length: 0\r\n\r\n"
        .to_vec()
}

/// Bob's 486 to that INVITE, its header block padded with `pad` bytes.
fn busy(pad: usize) -> Vec<u8> {
    format!(
        "SIP/2.0 486 Busy Here\r\nVia: SIP/2.0/UDP 127.0.0.1:47109;branch=z9hG4bK-inv1\r\n\
         Call-ID: cid-alloc\r\nFrom: <sip:peer@127.0.0.1>;tag=p1\r\n\
         To: <sip:bob@127.0.0.1>;tag=b1\r\nCSeq: 1 INVITE\r\nX-Pad: {}\r\n\
         Content-Length: 0\r\n\r\n",
        "p".repeat(pad)
    )
    .into_bytes()
}

/// The mux with bob's endpoint bound (`watched`: under every watcher and the
/// retransmit engine), `drop_rate` its loss, and the peer's plain endpoint.
struct Rig {
    _core: Arc<MuxCore>,
    _net: loadgen::mux::MuxNetwork,
    bob: Box<dyn UdpEndpoint>,
    peer: Box<dyn UdpEndpoint>,
    sim: Arc<SimulatedSignalingNetwork>,
}

async fn rig(drop_rate: f64, watched: bool) -> Rig {
    let sim = Arc::new(SimulatedSignalingNetwork::new(1));
    let clock = Clock::test_at(0);
    let core = MuxCore::bind_on(
        sim.as_ref(),
        vec![EndpointSpec { addr: addr(BOB), role: Role::Callee }],
        Correlation::header("X-Loadgen-Id"),
        64,
        0,
        RECV,
        clock.clone(),
    )
    .await
    .unwrap();
    let routing = CallRouting::new("tok-alloc").leg(addr(BOB), "bob");
    let mut net = core.network_tuned(routing, drop_rate, watched, 7, None);
    if watched {
        net = net
            .with_rtt_sink(Arc::new(|_, _| {}))
            .with_tally(Arc::new(CallTally::new("alice")))
            .with_wire_tap(CallWire::new(clock).tap());
    }
    let bob = net.bind_udp(BindUdpOpts::new(addr(BOB), 64)).await.unwrap();
    let peer = sim.bind_udp(BindUdpOpts::new(addr(PEER), 64)).await.unwrap();
    Rig { _core: core, _net: net, bob, peer, sim }
}

/// What bob's sending `raw` `n` times costs, after one send outside the count.
fn sends(rt: &Runtime, r: &Rig, raw: &[u8], n: usize) -> AllocCost {
    rt.block_on(async { r.bob.send_to(raw, addr(PEER)).await.unwrap() });
    let ((), cost) = measure(|| {
        rt.block_on(async {
            for _ in 0..n {
                r.bob.send_to(raw, addr(PEER)).await.unwrap();
            }
        })
    });
    cost
}

/// What the peer's sending `raw` `n` times to `to` costs, each one handled
/// there before the next.
fn arrivals(rt: &Runtime, r: &Rig, raw: &[u8], n: usize, to: u16) -> AllocCost {
    let ((), cost) = measure(|| {
        rt.block_on(async {
            for _ in 0..n {
                r.peer.send_to(raw, addr(to)).await.unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    });
    cost
}

/// A final every store keeps (the engine's re-answer and Timer G resender,
/// the late-answer hold) is held as one copy: the stores ask the allocator
/// for less than twice its size.
#[test]
#[ignore = "slow lane: loadgen alloc"]
fn a_final_the_stores_keep_is_copied_once() {
    let rt = runtime();
    // Loss 1: the datagram reaches the stores, never the fabric.
    let r = rt.block_on(rig(1.0, true));
    let raw = busy(16 * 1024);
    let ((), cost) =
        measure(|| rt.block_on(async { r.bob.send_to(&raw, addr(PEER)).await.unwrap() }));
    println!("a {}-byte final costs the stores {cost:?}", raw.len());
    assert!(
        cost.bytes < 2 * raw.len(),
        "the stores copied a {}-byte final more than once: {cost:?}",
        raw.len()
    );
}

/// A re-sent final costs a watched endpoint its one shared copy over what any
/// send costs: no watcher sniffs it again into owned text. Any send is an ACK
/// nothing keeps, on an endpoint nothing watches.
#[test]
#[ignore = "slow lane: loadgen alloc"]
fn a_resent_final_costs_its_one_shared_copy() {
    const N: usize = 32;
    let rt = runtime();
    let ack = b"ACK sip:peer@127.0.0.1 SIP/2.0\r\nCall-ID: cid-alloc\r\nCSeq: 1 ACK\r\n\r\n";
    let send = sends(&rt, &rt.block_on(rig(1.0, false)), ack, N);
    let resent = sends(&rt, &rt.block_on(rig(1.0, true)), &busy(0), N);
    println!("{N} sends: {send:?}; {N} re-sent finals, watched: {resent:?}");
    assert_eq!(resent.allocs, send.allocs + N, "one shared copy per datagram, nothing else");
}

/// A retransmitted request the engine absorbs costs the mux nothing: every
/// watcher reads the one sniff of it, and the arrival costs no more than the
/// fabric's own delivery to a plain endpoint.
#[test]
#[ignore = "slow lane: loadgen alloc"]
fn an_absorbed_duplicate_costs_the_mux_nothing() {
    const N: usize = 32;
    const PLAIN: u16 = 47120;
    let rt = runtime();
    let r = rt.block_on(rig(0.0, true));
    let raw = invite();
    rt.block_on(async {
        r.peer.send_to(&raw, addr(BOB)).await.unwrap();
        let got = tokio::time::timeout(RECV, r.bob.recv()).await.unwrap().unwrap();
        assert!(got.raw.starts_with(b"INVITE "));
    });
    // The fabric alone: a task taking each arrival off a plain endpoint, as
    // the mux's dispatcher takes it off the mux's.
    let plain = rt.block_on(r.sim.bind_udp(BindUdpOpts::new(addr(PLAIN), 64))).unwrap();
    rt.spawn(async move { while plain.recv().await.is_some() {} });
    let fabric = arrivals(&rt, &r, &raw, N, PLAIN);
    let mux = arrivals(&rt, &r, &raw, N, BOB);
    assert!(r.bob.try_recv().is_none(), "every duplicate absorbed");
    println!("{N} absorbed duplicates: {mux:?}; {N} plain arrivals: {fabric:?}");
    assert!(mux.allocs <= fabric.allocs, "the mux allocated for an absorbed duplicate");
}
