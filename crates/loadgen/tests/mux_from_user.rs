//! From-user correlation and a caller/callee shared socket on the mux,
//! exercised at the transport level over the simulated fabric under a paused
//! clock. A raw peer plays a SUT that sends each new leg back to the ip:port
//! that originated the call and keeps the calling party's From URI user (the
//! host, display name and tag may change). The tests assert exact demux
//! outcomes: which call owns which return leg, what is refused and counted,
//! and that the registry drains.

use std::net::SocketAddr;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use loadgen::{CallRouting, Correlation, EndpointSpec, MuxCore, Role};
use sip_clock::Clock;
use sip_net::{BindUdpOpts, SignalingNetwork, SimulatedSignalingNetwork, UdpEndpoint};

const RECV: Duration = Duration::from_secs(20);

fn addr(p: u16) -> SocketAddr {
    format!("127.0.0.1:{p}").parse().unwrap()
}

/// The caller's initial INVITE toward the SUT, calling party `from_user`.
fn caller_invite(call_id: &str, from_user: &str, shared: SocketAddr) -> Vec<u8> {
    format!(
        "INVITE sip:+15550900@127.0.0.1:9 SIP/2.0\r\n\
         Via: SIP/2.0/UDP {shared};branch=z9hG4bK-{call_id}\r\nMax-Forwards: 70\r\n\
         Call-ID: {call_id}\r\nFrom: <sip:{from_user}@{shared}>;tag=a-{call_id}\r\n\
         To: <sip:+15550900@127.0.0.1>\r\nCSeq: 1 INVITE\r\n\
         Contact: <sip:{from_user}@{shared}>\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// A leg the SUT originates back to the caller's socket: fresh Call-ID and
/// branch, the SUT's own host, display name and tag on a From that keeps the
/// calling party's user, and the routed number in the R-URI and To.
fn return_invite(ruri_user: &str, call_id: &str, from_user: &str) -> Vec<u8> {
    format!(
        "INVITE sip:{ruri_user}@127.0.0.1 SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:9;branch=z9hG4bK-{call_id}\r\nMax-Forwards: 69\r\n\
         Call-ID: {call_id}\r\n\
         From: \"Relayed\" <sip:{from_user}@sut.example:5080;user=phone>;tag=s-{call_id}\r\n\
         To: <sip:{ruri_user}@127.0.0.1>\r\nCSeq: 1 INVITE\r\n\
         Contact: <sip:sut@127.0.0.1:9>\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// An in-dialog BYE from the SUT on an established return leg (no token).
fn bye(call_id: &str, from_user: &str) -> Vec<u8> {
    format!(
        "BYE sip:leg@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:9;branch=z9hG4bK-b{call_id}\r\n\
         Max-Forwards: 70\r\nCall-ID: {call_id}\r\n\
         From: <sip:{from_user}@sut.example>;tag=s-{call_id}\r\nTo: <sip:leg@h>;tag=l1\r\n\
         CSeq: 2 BYE\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

async fn setup(
    shared: SocketAddr,
    sut_port: u16,
) -> (Arc<SimulatedSignalingNetwork>, Arc<MuxCore>, Box<dyn UdpEndpoint>) {
    let sim = Arc::new(SimulatedSignalingNetwork::new(1));
    let core = MuxCore::bind_on(
        sim.as_ref(),
        vec![EndpointSpec { addr: shared, role: Role::Caller }],
        Correlation::from_user(),
        64,
        8,
        RECV,
        Clock::test_at(0),
    )
    .await
    .unwrap();
    let sut = sim.bind_udp(BindUdpOpts::new(addr(sut_port), 64)).await.unwrap();
    (sim, core, sut)
}

/// The first line of the next datagram `ep` receives, `None` on timeout.
async fn next_line(ep: &dyn UdpEndpoint) -> Option<String> {
    let pkt = tokio::time::timeout(RECV, ep.recv()).await.ok()??;
    Some(String::from_utf8_lossy(&pkt.raw).lines().next().unwrap_or("").to_string())
}

fn orphans(core: &MuxCore) -> u64 {
    let s = core.stats();
    s.orphan_no_header.load(Relaxed)
        + s.orphan_unknown_token.load(Relaxed)
        + s.orphan_stray.load(Relaxed)
}

/// A layout whose caller and callee roles name one address opens ONE fabric
/// endpoint there (a second bind of the same address would be refused).
#[tokio::test(start_paused = true)]
async fn a_layout_with_every_role_on_one_address_opens_one_endpoint() {
    let sim = SimulatedSignalingNetwork::new(1);
    let shared = addr(47001);
    let core = MuxCore::bind_on(
        &sim,
        vec![
            EndpointSpec { addr: shared, role: Role::Caller },
            EndpointSpec { addr: shared, role: Role::Callee },
            EndpointSpec { addr: shared, role: Role::Callee },
        ],
        Correlation::from_user(),
        64,
        8,
        RECV,
        Clock::test_at(0),
    )
    .await;
    assert!(core.is_ok(), "a shared layout must open, got {:?}", core.err());
    assert_eq!(core.unwrap().addrs(), vec![shared], "one endpoint per distinct address");
}

/// Two overlapping calls on one shared socket, keyed by their callers' From
/// users: each return leg reaches its own call's callee although its From
/// carries the SUT's host, display name and tag; the callers keep their
/// response path by Call-ID; in-dialog BYEs reach each leg by Call-ID; the
/// registry drains to 0.
#[tokio::test(start_paused = true)]
async fn return_legs_reach_their_own_call_by_from_user_on_a_shared_socket() {
    let shared = addr(47101);
    let (_sim, core, sut) = setup(shared, 47109).await;
    let sut_addr = sut.local_addr();

    let net_a = core.network(CallRouting::new("+1555010").caller(shared).leg(shared, "bob"));
    let alice_a = net_a.bind_udp(BindUdpOpts::new(shared, 16)).await.unwrap();
    let bob_a = net_a.bind_udp(BindUdpOpts::new(shared, 16)).await.unwrap();
    let net_b = core.network(CallRouting::new("+1555011").caller(shared).leg(shared, "bob"));
    let alice_b = net_b.bind_udp(BindUdpOpts::new(shared, 16)).await.unwrap();
    let bob_b = net_b.bind_udp(BindUdpOpts::new(shared, 16)).await.unwrap();

    alice_a.send_to(&caller_invite("cid-a", "+1555010", shared), sut_addr).await.unwrap();
    alice_b.send_to(&caller_invite("cid-b", "+1555011", shared), sut_addr).await.unwrap();
    for _ in 0..2 {
        assert!(next_line(sut.as_ref()).await.is_some_and(|l| l.starts_with("INVITE")));
    }

    // The return legs arrive in the reverse order of the calls.
    sut.send_to(&return_invite("+15550900", "ret-b", "+1555011"), shared).await.unwrap();
    sut.send_to(&return_invite("+15550900", "ret-a", "+1555010"), shared).await.unwrap();
    assert_eq!(
        next_line(bob_b.as_ref()).await.as_deref(),
        Some("INVITE sip:+15550900@127.0.0.1 SIP/2.0"),
        "call B's return leg must reach call B's callee",
    );
    assert_eq!(
        next_line(bob_a.as_ref()).await.as_deref(),
        Some("INVITE sip:+15550900@127.0.0.1 SIP/2.0"),
        "call A's return leg must reach call A's callee",
    );

    // A response on each caller's own dialog still reaches that caller.
    sut.send_to(
        b"SIP/2.0 180 Ringing\r\nVia: SIP/2.0/UDP 127.0.0.1:47101;branch=z9hG4bK-cid-a\r\n\
          Call-ID: cid-a\r\nFrom: <sip:+1555010@127.0.0.1:47101>;tag=a-cid-a\r\n\
          To: <sip:+15550900@127.0.0.1>;tag=u1\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n",
        shared,
    )
    .await
    .unwrap();
    assert_eq!(next_line(alice_a.as_ref()).await.as_deref(), Some("SIP/2.0 180 Ringing"));

    sut.send_to(&bye("ret-a", "+1555010"), shared).await.unwrap();
    sut.send_to(&bye("ret-b", "+1555011"), shared).await.unwrap();
    assert_eq!(next_line(bob_a.as_ref()).await.as_deref(), Some("BYE sip:leg@127.0.0.1 SIP/2.0"));
    assert_eq!(next_line(bob_b.as_ref()).await.as_deref(), Some("BYE sip:leg@127.0.0.1 SIP/2.0"));

    assert_eq!(orphans(&core), 0, "orphans: {:?}", core.stats().samples());
    drop((alice_a, bob_a, alice_b, bob_b));
    assert_eq!(core.registry_size(), 0, "mux registry leak");
}

/// A second leg the SUT originates for the same call (the primary rejected,
/// the alternate is dialled) carries the same From user on a new Call-ID and
/// reaches the call's second receiver, selected by the call's picker.
#[tokio::test(start_paused = true)]
async fn a_second_return_leg_of_the_same_call_reaches_that_call() {
    let shared = addr(47201);
    let (_sim, core, sut) = setup(shared, 47209).await;
    let sut_addr = sut.local_addr();

    let routing =
        CallRouting::new("+1555012").caller(shared).leg(shared, "bob").leg(shared, "bob2").picker(
            shared,
            loadgen::labelled_prefix_leg_picker([("+155509", "bob"), ("bob2", "bob2")]),
        );
    let net = core.network(routing);
    let alice = net.bind_udp(BindUdpOpts::new(shared, 16)).await.unwrap();
    let bob = net.bind_udp(BindUdpOpts::new(shared, 16)).await.unwrap();
    let bob2 = net.bind_udp(BindUdpOpts::new(shared, 16)).await.unwrap();

    alice.send_to(&caller_invite("cid-r", "+1555012", shared), sut_addr).await.unwrap();
    assert!(next_line(sut.as_ref()).await.is_some_and(|l| l.starts_with("INVITE")));

    sut.send_to(&return_invite("+15550900", "ret-1", "+1555012"), shared).await.unwrap();
    assert_eq!(
        next_line(bob.as_ref()).await.as_deref(),
        Some("INVITE sip:+15550900@127.0.0.1 SIP/2.0"),
        "the primary leg reaches the call's first receiver",
    );
    sut.send_to(&return_invite("bob2", "ret-2", "+1555012"), shared).await.unwrap();
    assert_eq!(
        next_line(bob2.as_ref()).await.as_deref(),
        Some("INVITE sip:bob2@127.0.0.1 SIP/2.0"),
        "the alternate leg reaches the same call's second receiver",
    );

    sut.send_to(&bye("ret-2", "+1555012"), shared).await.unwrap();
    assert_eq!(next_line(bob2.as_ref()).await.as_deref(), Some("BYE sip:leg@127.0.0.1 SIP/2.0"));
    assert_eq!(orphans(&core), 0, "orphans: {:?}", core.stats().samples());
    drop((alice, bob, bob2));
    assert_eq!(core.registry_size(), 0, "mux registry leak");
}

/// Under from-user correlation the caller's first INVITE must carry the call's
/// key as its From user: a different user fails the send, is counted, and
/// never reaches the wire; a matching one goes out.
#[tokio::test(start_paused = true)]
async fn a_caller_invite_whose_from_user_is_not_the_key_is_refused_before_the_wire() {
    let shared = addr(47301);
    let (_sim, core, sut) = setup(shared, 47309).await;
    let sut_addr = sut.local_addr();

    let net = core.network(CallRouting::new("+1555013").caller(shared).leg(shared, "bob"));
    let alice = net.bind_udp(BindUdpOpts::new(shared, 16)).await.unwrap();
    let bob = net.bind_udp(BindUdpOpts::new(shared, 16)).await.unwrap();

    let sent = alice.send_to(&caller_invite("cid-m", "+15550199", shared), sut_addr).await;
    assert!(sent.is_err(), "a From user other than the key must fail the send");
    assert_eq!(core.stats().caller_key_mismatch.load(Relaxed), 1);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(sut.try_recv().is_none(), "the refused INVITE reached the wire");

    let net_ok = core.network(CallRouting::new("+1555014").caller(shared).leg(shared, "bob"));
    let alice_ok = net_ok.bind_udp(BindUdpOpts::new(shared, 16)).await.unwrap();
    let bob_ok = net_ok.bind_udp(BindUdpOpts::new(shared, 16)).await.unwrap();
    alice_ok.send_to(&caller_invite("cid-k", "+1555014", shared), sut_addr).await.unwrap();
    assert!(next_line(sut.as_ref()).await.is_some_and(|l| l.starts_with("INVITE")));
    assert_eq!(core.stats().caller_key_mismatch.load(Relaxed), 1);

    drop((alice, bob, alice_ok, bob_ok));
    assert_eq!(core.registry_size(), 0, "mux registry leak");
}
