//! Scenario-defined claims on the mux, exercised at the transport level over
//! the simulated fabric under a paused clock: a raw peer plays the SUT side
//! (sending the correlated INVITEs a B2BUA would originate) and the tests
//! assert exact demux outcomes — which receiver owns which leg, what is
//! counted `unclaimed` vs orphan, and that a colliding token registration
//! fails at bind rather than misdelivering a leg.

use std::net::SocketAddr;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use loadgen::{CallRouting, ClaimRule, Correlation, EndpointSpec, MuxCore, Role};
use sip_clock::Clock;
use sip_net::{BindUdpOpts, SignalingNetwork, SimulatedSignalingNetwork, UdpEndpoint};

const RECV: Duration = Duration::from_secs(20);

fn addr(p: u16) -> SocketAddr {
    format!("127.0.0.1:{p}").parse().unwrap()
}

/// An initial INVITE from the simulated SUT: fresh Call-ID, the relayed
/// correlation header, and optional extra header lines (e.g. `Replaces`).
fn invite(ruri: &str, call_id: &str, token: &str, extra: &str) -> Vec<u8> {
    format!(
        "INVITE {ruri} SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:9\r\nCall-ID: {call_id}\r\n\
         X-Loadgen-Id: {token}\r\n{extra}To: <{ruri}>\r\nFrom: <sip:sut@h>;tag=s1\r\n\
         CSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// An in-dialog request on an established leg (same Call-ID, NO token header —
/// the known-Call-ID tier must carry it alone).
fn in_dialog(method: &str, call_id: &str) -> Vec<u8> {
    format!(
        "{method} sip:leg@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:9\r\n\
         Call-ID: {call_id}\r\nTo: <sip:leg@h>;tag=l1\r\nFrom: <sip:sut@h>;tag=s1\r\n\
         CSeq: 2 {method}\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

async fn setup(base: u16, specs: Vec<EndpointSpec>) -> (Arc<SimulatedSignalingNetwork>, Arc<MuxCore>, Box<dyn UdpEndpoint>) {
    let sim = Arc::new(SimulatedSignalingNetwork::new(1));
    let core = MuxCore::bind_on(
        sim.as_ref(),
        specs,
        Correlation::header("X-Loadgen-Id"),
        64,
        8,
        RECV,
        Clock::test_at(0),
    )
    .await
    .unwrap();
    let sut = sim.bind_udp(BindUdpOpts::new(addr(base + 9), 64)).await.unwrap();
    (sim, core, sut)
}

async fn recv_first_line(ep: &dyn UdpEndpoint) -> String {
    let pkt = tokio::time::timeout(RECV, ep.recv()).await.expect("recv timed out").expect("queue closed");
    String::from_utf8_lossy(&pkt.raw).lines().next().unwrap_or("").to_string()
}

/// The 050 multi-actor shape on ONE shared UAS socket: an MRF leg claimed by
/// its service number, a transfer-target leg claimed by Replaces, and a
/// same-number primary/alternate pair claimed by arrival order — with the legs
/// arriving in an order that interleaves the specific claims between the
/// order claims, proving specifically-claimed legs never shift an ordinal.
/// After claim time, in-dialog requests (no token header) reach each leg by
/// Call-ID alone.
#[tokio::test(start_paused = true)]
async fn claims_assign_reroute_transfer_and_mrf_legs_on_one_socket() {
    let uas = addr(46101);
    let (_sim, core, sut) = setup(46100, vec![EndpointSpec { addr: uas, role: Role::Callee }]).await;

    let routing = CallRouting::new("lgcase1")
        .claim(uas, "primary", ClaimRule::ArrivalOrder(0))
        .claim(uas, "alternate", ClaimRule::ArrivalOrder(1))
        .claim(uas, "mrf", ClaimRule::RuriUser("0491".into()))
        .claim(uas, "xfer", ClaimRule::HasReplaces);
    let net = core.network(routing);
    let primary = net.bind_udp(BindUdpOpts::new(uas, 16)).await.unwrap();
    let alternate = net.bind_udp(BindUdpOpts::new(uas, 16)).await.unwrap();
    let mrf = net.bind_udp(BindUdpOpts::new(uas, 16)).await.unwrap();
    let xfer = net.bind_udp(BindUdpOpts::new(uas, 16)).await.unwrap();

    // Same callee number on primary and alternate (the reroute capture shape);
    // the MRF and Replaces legs interleave between them.
    sut.send_to(&invite("sip:0590100@127.0.0.1", "cid-p", "lgcase1", ""), uas).await.unwrap();
    assert_eq!(recv_first_line(primary.as_ref()).await, "INVITE sip:0590100@127.0.0.1 SIP/2.0");
    sut.send_to(&invite("sip:049177@127.0.0.1", "cid-m", "lgcase1", ""), uas).await.unwrap();
    assert_eq!(recv_first_line(mrf.as_ref()).await, "INVITE sip:049177@127.0.0.1 SIP/2.0");
    sut.send_to(
        &invite("sip:0590100@127.0.0.1", "cid-x", "lgcase1", "Replaces: cid-p;to-tag=1;from-tag=2\r\n"),
        uas,
    )
    .await
    .unwrap();
    assert_eq!(recv_first_line(xfer.as_ref()).await, "INVITE sip:0590100@127.0.0.1 SIP/2.0");
    // The alternate arrives LAST yet still takes ordinal 1: the mrf and xfer
    // legs were claimed specifically and consumed no ordinal.
    sut.send_to(&invite("sip:0590100@127.0.0.1", "cid-a", "lgcase1", ""), uas).await.unwrap();
    assert_eq!(recv_first_line(alternate.as_ref()).await, "INVITE sip:0590100@127.0.0.1 SIP/2.0");

    // Everything in-dialog the new legs generate demuxes by Call-ID alone
    // (scenario code sees no demux concern after claim time): CANCEL on the
    // abandoned primary, INFO on the MRF dialog, BYE on the transfer leg.
    sut.send_to(&in_dialog("CANCEL", "cid-p"), uas).await.unwrap();
    assert_eq!(recv_first_line(primary.as_ref()).await, "CANCEL sip:leg@127.0.0.1 SIP/2.0");
    sut.send_to(&in_dialog("INFO", "cid-m"), uas).await.unwrap();
    assert_eq!(recv_first_line(mrf.as_ref()).await, "INFO sip:leg@127.0.0.1 SIP/2.0");
    sut.send_to(&in_dialog("BYE", "cid-x"), uas).await.unwrap();
    assert_eq!(recv_first_line(xfer.as_ref()).await, "BYE sip:leg@127.0.0.1 SIP/2.0");

    let s = core.stats();
    assert_eq!(s.unclaimed.load(Relaxed), 0);
    assert_eq!(s.orphan_no_header.load(Relaxed) + s.orphan_unknown_token.load(Relaxed) + s.orphan_stray.load(Relaxed), 0);

    drop((primary, alternate, mrf, xfer));
    assert_eq!(core.registry_size(), 0, "mux registry leak");
    assert_eq!(core.stats().claim_unfired.load(Relaxed), 0);
}

/// The pilot's mux vantage: the SUT dials a NEW call back to the same ip:port
/// that originated leg A. The caller and a pending Replaces claim share the
/// socket via explicit `caller()`/`claim()` declarations; the dial-back INVITE
/// lands on the claim receiver while the caller's own dialog traffic keeps
/// demuxing by Call-ID.
#[tokio::test(start_paused = true)]
async fn dialback_invite_lands_on_claim_receiver_sharing_the_originating_socket() {
    let vantage = addr(46201);
    let (_sim, core, sut) =
        setup(46200, vec![EndpointSpec { addr: vantage, role: Role::Caller }]).await;

    let routing = CallRouting::new("lgcase2")
        .caller(vantage)
        .claim(vantage, "xferee", ClaimRule::HasReplaces);
    let net = core.network(routing);
    let alice = net.bind_udp(BindUdpOpts::new(vantage, 16)).await.unwrap();
    let xferee = net.bind_udp(BindUdpOpts::new(vantage, 16)).await.unwrap();

    // Leg A: alice originates; her response comes back by Call-ID.
    let sut_addr = sut.local_addr();
    alice
        .send_to(&invite("sip:0590200@127.0.0.1", "cid-alice", "lgcase2", ""), sut_addr)
        .await
        .unwrap();
    let seen = tokio::time::timeout(RECV, sut.recv()).await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&seen.raw).starts_with("INVITE"));
    sut.send_to(
        b"SIP/2.0 180 Ringing\r\nVia: SIP/2.0/UDP 127.0.0.1:9\r\nCall-ID: cid-alice\r\n\
          To: <sip:0590200@h>;tag=u1\r\nFrom: <sip:sut@h>;tag=s1\r\nCSeq: 1 INVITE\r\n\
          Content-Length: 0\r\n\r\n",
        vantage,
    )
    .await
    .unwrap();
    assert_eq!(recv_first_line(alice.as_ref()).await, "SIP/2.0 180 Ringing");

    // The SUT dials BACK to the originating socket: a NEW INVITE, new Call-ID,
    // same relayed token, carrying Replaces — it must land on the pending
    // claim, not on alice, and its in-dialog follow-up demuxes by Call-ID.
    sut.send_to(
        &invite("sip:0590200@127.0.0.1", "cid-back", "lgcase2", "Replaces: cid-alice;to-tag=u1;from-tag=s1\r\n"),
        vantage,
    )
    .await
    .unwrap();
    assert_eq!(recv_first_line(xferee.as_ref()).await, "INVITE sip:0590200@127.0.0.1 SIP/2.0");
    sut.send_to(&in_dialog("BYE", "cid-back"), vantage).await.unwrap();
    assert_eq!(recv_first_line(xferee.as_ref()).await, "BYE sip:leg@127.0.0.1 SIP/2.0");

    let s = core.stats();
    assert_eq!(s.unclaimed.load(Relaxed), 0);
    assert_eq!(s.orphan_no_header.load(Relaxed) + s.orphan_unknown_token.load(Relaxed) + s.orphan_stray.load(Relaxed), 0);

    drop((alice, xferee));
    assert_eq!(core.registry_size(), 0, "mux registry leak");
}

/// Claims are scoped to the call instance, never global: a SECOND call
/// registering the same token (To-user correlation + intentionally shared
/// callee numbers) fails its bind explicitly and is counted — the residual
/// ambiguity is a visible failure, not a misdelivered leg. The FIRST call's
/// claim keeps working.
#[tokio::test(start_paused = true)]
async fn colliding_token_registration_fails_bind_and_counts() {
    let uas = addr(46301);
    let (_sim, core, sut) = setup(46300, vec![EndpointSpec { addr: uas, role: Role::Callee }]).await;

    let net1 = core.network(
        CallRouting::new("0590300").claim(uas, "bob", ClaimRule::RuriUser("0590300".into())),
    );
    let bob = net1.bind_udp(BindUdpOpts::new(uas, 16)).await.unwrap();

    let net2 = core.network(
        CallRouting::new("0590300").claim(uas, "bob", ClaimRule::RuriUser("0590300".into())),
    );
    let err = net2.bind_udp(BindUdpOpts::new(uas, 16)).await.err().expect("colliding bind must fail");
    assert!(err.message.contains("already registered by a concurrent call"), "unexpected error: {err}");
    assert_eq!(core.stats().token_collision.load(Relaxed), 1);

    sut.send_to(&invite("sip:0590300@127.0.0.1", "cid-b", "0590300", ""), uas).await.unwrap();
    assert_eq!(recv_first_line(bob.as_ref()).await, "INVITE sip:0590300@127.0.0.1 SIP/2.0");

    drop(bob);
    assert_eq!(core.registry_size(), 0, "mux registry leak");
}

/// A correlated INVITE no pending claim accepts is counted `unclaimed` —
/// separately from true orphans — and consumed claims never re-fire: the same
/// role number arriving again on a NEW Call-ID goes unclaimed instead of
/// stealing the established leg's receiver. An unfired claim is counted on
/// release.
#[tokio::test(start_paused = true)]
async fn unclaimed_invites_and_unfired_claims_are_counted_not_misdelivered() {
    let uas = addr(46401);
    let (_sim, core, sut) = setup(46400, vec![EndpointSpec { addr: uas, role: Role::Callee }]).await;

    let routing = CallRouting::new("lgcase4")
        .claim(uas, "bob", ClaimRule::RuriUser("0590400".into()))
        .claim(uas, "never", ClaimRule::HasReplaces);
    let net = core.network(routing);
    let bob = net.bind_udp(BindUdpOpts::new(uas, 16)).await.unwrap();
    let never = net.bind_udp(BindUdpOpts::new(uas, 16)).await.unwrap();

    // Correct token, unknown role number → unclaimed (a KNOWN call's leg the
    // scenario did not declare), while a token-less INVITE stays a true orphan.
    sut.send_to(&invite("sip:999@127.0.0.1", "cid-u1", "lgcase4", ""), uas).await.unwrap();
    sut.send_to(
        b"INVITE sip:0590400@127.0.0.1 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:9\r\n\
          Call-ID: cid-nohdr\r\nTo: <sip:0590400@h>\r\nFrom: <sip:sut@h>;tag=s1\r\n\
          CSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n",
        uas,
    )
    .await
    .unwrap();
    // The declared leg claims fine after the stray...
    sut.send_to(&invite("sip:0590400@127.0.0.1", "cid-b", "lgcase4", ""), uas).await.unwrap();
    assert_eq!(recv_first_line(bob.as_ref()).await, "INVITE sip:0590400@127.0.0.1 SIP/2.0");
    // ...and its claim is consumed: the same number on a NEW Call-ID is
    // unclaimed, not delivered to bob again as a fresh leg.
    sut.send_to(&invite("sip:0590400@127.0.0.1", "cid-b2", "lgcase4", ""), uas).await.unwrap();
    sut.send_to(&in_dialog("BYE", "cid-b"), uas).await.unwrap();
    assert_eq!(recv_first_line(bob.as_ref()).await, "BYE sip:leg@127.0.0.1 SIP/2.0");

    let s = core.stats();
    assert_eq!(s.unclaimed.load(Relaxed), 2);
    assert_eq!(s.orphan_no_header.load(Relaxed), 1);
    assert_eq!(s.orphan_unknown_token.load(Relaxed), 0);

    // The Replaces claim never fired: counted on release, and the registry
    // still drains fully.
    drop((bob, never));
    assert_eq!(s.claim_unfired.load(Relaxed), 1);
    assert_eq!(core.registry_size(), 0, "mux registry leak");
}

/// Config errors fail the bind loudly: mixing claim and non-claim legs on one
/// token slot, declaring both claims and a picker on a socket, and binding
/// more endpoints than declared.
#[tokio::test(start_paused = true)]
async fn invalid_claim_configurations_fail_bind() {
    let uas = addr(46501);
    let (_sim, core, _sut) = setup(46500, vec![EndpointSpec { addr: uas, role: Role::Callee }]).await;

    // Claim + legacy leg on one slot.
    let net = core.network(
        CallRouting::new("lgmix")
            .claim(uas, "a", ClaimRule::ArrivalOrder(0))
            .leg(uas, "b"),
    );
    let _a = net.bind_udp(BindUdpOpts::new(uas, 16)).await.unwrap();
    let err = net.bind_udp(BindUdpOpts::new(uas, 16)).await.err().expect("mixed slot must fail");
    assert!(err.message.contains("mixed claim and non-claim"), "unexpected error: {err}");

    // Claims + picker on one socket.
    let net = core.network(
        CallRouting::new("lgpick")
            .claim(uas, "a", ClaimRule::ArrivalOrder(0))
            .picker(uas, loadgen::prefix_leg_picker(["a"])),
    );
    let err = net.bind_udp(BindUdpOpts::new(uas, 16)).await.err().expect("claims+picker must fail");
    assert!(err.message.contains("not both"), "unexpected error: {err}");

    // More binds than declared.
    let net = core.network(CallRouting::new("lgover").claim(uas, "a", ClaimRule::ArrivalOrder(0)));
    let _a = net.bind_udp(BindUdpOpts::new(uas, 16)).await.unwrap();
    let err = net.bind_udp(BindUdpOpts::new(uas, 16)).await.err().expect("overbind must fail");
    assert!(err.message.contains("more times than declared"), "unexpected error: {err}");
}
