//! The INVITE client transaction outlives the call that released it (RFC 3261
//! §17.1.1.3): a non-2xx final arriving after the caller endpoint dropped is
//! ACKed from the mux, each retransmission of it too, and none is an orphan.
//! A raw peer plays the SUT over the simulated fabric under a paused clock.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use loadgen::{CallRouting, Correlation, EndpointSpec, MuxCore, Role};
use sip_clock::Clock;
use sip_message::sniff::{
    call_id, cseq_value, header_value, header_values, request_uri, via_branch,
};
use sip_net::{BindUdpOpts, SignalingNetwork, SimulatedSignalingNetwork, UdpEndpoint};

const RECV: Duration = Duration::from_secs(20);
const BRANCH: &str = "z9hG4bK-late-1";

fn addr(p: u16) -> SocketAddr {
    format!("127.0.0.1:{p}").parse().unwrap()
}

/// The caller's initial INVITE, preloaded with a Route to the SUT.
fn invite(call_id: &str, sut: SocketAddr) -> Vec<u8> {
    format!(
        "INVITE sip:callee@{sut} SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:46301;branch={BRANCH}\r\n\
         Max-Forwards: 70\r\nRoute: <sip:{sut};lr>\r\nCall-ID: {call_id}\r\n\
         X-Loadgen-Id: tok-late\r\nFrom: <sip:caller@h>;tag=c1\r\nTo: <sip:callee@h>\r\n\
         CSeq: 7 INVITE\r\nContact: <sip:caller@127.0.0.1:46301>\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// The SUT's final to that INVITE.
fn final_response(status: &str, call_id: &str, cseq: u32) -> Vec<u8> {
    format!(
        "SIP/2.0 {status}\r\nVia: SIP/2.0/UDP 127.0.0.1:46301;branch={BRANCH}\r\n\
         Call-ID: {call_id}\r\nFrom: <sip:caller@h>;tag=c1\r\nTo: <sip:callee@h>;tag=u9\r\n\
         CSeq: {cseq} INVITE\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

async fn setup() -> (Arc<SimulatedSignalingNetwork>, Arc<MuxCore>, Box<dyn UdpEndpoint>) {
    let sim = Arc::new(SimulatedSignalingNetwork::new(1));
    let core = MuxCore::bind_on(
        sim.as_ref(),
        vec![EndpointSpec { addr: addr(46301), role: Role::Caller }],
        Correlation::header("X-Loadgen-Id"),
        64,
        8,
        RECV,
        Clock::test_at(0),
    )
    .await
    .unwrap();
    let sut = sim.bind_udp(BindUdpOpts::new(addr(46309), 64)).await.unwrap();
    (sim, core, sut)
}

/// The caller sends its INVITE (seen by the SUT), then its call ends and
/// releases every endpoint before any final arrived.
async fn released_call(core: &Arc<MuxCore>, sut: &dyn UdpEndpoint, call_id: &str) {
    let net = core.network(CallRouting::new("tok-late"));
    let caller = net.bind_udp(BindUdpOpts::new(addr(46301), 16)).await.unwrap();
    caller.send_to(&invite(call_id, sut.local_addr()), sut.local_addr()).await.unwrap();
    let seen = tokio::time::timeout(RECV, sut.recv()).await.unwrap().unwrap();
    assert!(seen.raw.starts_with(b"INVITE "));
    drop(caller);
    drop(net);
}

async fn recv(sut: &dyn UdpEndpoint) -> Option<Vec<u8>> {
    tokio::time::timeout(Duration::from_secs(2), sut.recv()).await.ok().flatten().map(|p| p.raw)
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_late_487_and_its_retransmission_are_acked_and_never_orphans() {
    let (_sim, core, sut) = setup().await;
    released_call(&core, sut.as_ref(), "cid-late-487").await;

    sut.send_to(&final_response("487 Request Terminated", "cid-late-487", 7), addr(46301))
        .await
        .unwrap();
    let ack = recv(sut.as_ref()).await.expect("the late 487 is ACKed");
    let text = String::from_utf8_lossy(&ack).to_string();
    let sut_addr = sut.local_addr();
    assert_eq!(request_uri(&ack).as_deref(), Some(format!("sip:callee@{sut_addr}").as_str()));
    assert!(text.starts_with("ACK "), "{text}");
    assert_eq!(via_branch(&ack).as_deref(), Some(BRANCH), "the INVITE's branch: {text}");
    assert_eq!(cseq_value(&ack), "CSeq: 7 ACK");
    assert_eq!(call_id(&ack).as_deref(), Some("cid-late-487"));
    assert!(header_value(&ack, "To").is_some_and(|to| to.contains("tag=u9")), "{text}");
    assert_eq!(header_values(&ack, "Route"), vec![format!("<sip:{sut_addr};lr>")]);

    sut.send_to(&final_response("487 Request Terminated", "cid-late-487", 7), addr(46301))
        .await
        .unwrap();
    let again = recv(sut.as_ref()).await.expect("the retransmitted 487 is ACKed");
    assert_eq!(again, ack, "a retransmission draws the same ACK");
    assert_eq!(core.stats().orphans_total(), 0, "a late final of a released call is no orphan");
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_late_final_of_another_transaction_or_after_the_hold_is_an_orphan() {
    let (_sim, core, sut) = setup().await;
    released_call(&core, sut.as_ref(), "cid-late-x").await;

    // Another CSeq: not the released INVITE's transaction.
    sut.send_to(&final_response("486 Busy Here", "cid-late-x", 8), addr(46301)).await.unwrap();
    assert!(recv(sut.as_ref()).await.is_none(), "no ACK for a final of no known INVITE");
    assert_eq!(core.stats().orphans_total(), 1);

    // Past the hold, the transaction is gone (Timer D).
    tokio::time::sleep(loadgen::mux::RELEASE_HOLD + Duration::from_secs(6)).await;
    sut.send_to(&final_response("486 Busy Here", "cid-late-x", 7), addr(46301)).await.unwrap();
    assert!(recv(sut.as_ref()).await.is_none(), "no ACK once the transaction is gone");
    assert_eq!(core.stats().orphans_total(), 2);
}

/// An INVITE a 2xx answered is not held past its call: the dialog owns that
/// ACK, so a released call answered 2xx keeps nothing for a later final.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn an_invite_answered_2xx_is_not_held_after_release() {
    let (_sim, core, sut) = setup().await;
    let net = core.network(CallRouting::new("tok-late"));
    let caller = net.bind_udp(BindUdpOpts::new(addr(46301), 16)).await.unwrap();
    caller.send_to(&invite("cid-answered", sut.local_addr()), sut.local_addr()).await.unwrap();
    assert!(recv(sut.as_ref()).await.is_some_and(|raw| raw.starts_with(b"INVITE ")));
    sut.send_to(&final_response("200 OK", "cid-answered", 7), addr(46301)).await.unwrap();
    let ok = tokio::time::timeout(RECV, caller.recv()).await.unwrap().unwrap();
    assert!(ok.raw.starts_with(b"SIP/2.0 200"));
    drop(caller);
    drop(net);

    sut.send_to(&final_response("487 Request Terminated", "cid-answered", 7), addr(46301))
        .await
        .unwrap();
    assert!(recv(sut.as_ref()).await.is_none(), "nothing held for an answered INVITE");
    assert_eq!(core.stats().orphans_total(), 1);
}
