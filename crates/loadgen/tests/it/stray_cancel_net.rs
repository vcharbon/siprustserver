//! A CANCEL for a callee the loadgen already released is answered, never left
//! to its Timer F (RFC 3261 §9.2): a 200 when the INVITE's final is still
//! held, else a 481 built from the CANCEL alone, the same on every
//! retransmission. Neither is an orphan.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use loadgen::{CallRouting, Correlation, EndpointSpec, MuxCore, Role};
use sip_message::sniff::identity;
use sip_net::{BindUdpOpts, SignalingNetwork, UdpEndpoint};
use sip_net_testkit::SimulatedSignalingNetwork;

use crate::fake_rig::*;

const BOB: u16 = 46501;
const SUT: u16 = 46509;

/// A raw SUT request on Call-ID `cid` to the callee endpoint, To with no tag.
fn sut_request(method: &str, cid: &str, cseq: &str) -> Vec<u8> {
    format!(
        "{method} sip:bob@127.0.0.1:{BOB} SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:{SUT};branch=z9hG4bK-{cid}\r\n\
         Max-Forwards: 70\r\nCall-ID: {cid}\r\nX-Loadgen-Id: tok-{cid}\r\n\
         From: <sip:sut@h>;tag=s1\r\nTo: <sip:bob@h>\r\nCSeq: {cseq}\r\n\
         Contact: <sip:sut@127.0.0.1:{SUT}>\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// bob's response `status` to the SUT's INVITE on `cid`, To tag `b1`.
fn bob_response(status: &str, cid: &str) -> Vec<u8> {
    format!(
        "SIP/2.0 {status}\r\nVia: SIP/2.0/UDP 127.0.0.1:{SUT};branch=z9hG4bK-{cid}\r\n\
         Call-ID: {cid}\r\nFrom: <sip:sut@h>;tag=s1\r\nTo: <sip:bob@h>;tag=b1\r\n\
         CSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// The simulated network, a callee mux on `BOB` on it and the SUT's socket on
/// `SUT`.
async fn bind() -> (Arc<SimulatedSignalingNetwork>, Arc<MuxCore>, Box<dyn UdpEndpoint>) {
    let sim = Arc::new(SimulatedSignalingNetwork::new(1));
    let core = MuxCore::bind_on(
        sim.as_ref(),
        vec![EndpointSpec { addr: addr(BOB), role: Role::Callee }],
        Correlation::header("X-Loadgen-Id"),
        64,
        8,
        RECV,
        sip_clock::Clock::test_at(0),
    )
    .await
    .unwrap();
    let sut = sim.bind_udp(BindUdpOpts::new(addr(SUT), 64)).await.unwrap();
    (sim, core, sut)
}

async fn recv(sut: &dyn UdpEndpoint) -> Option<Vec<u8>> {
    let p = tokio::time::timeout(Duration::from_secs(2), sut.recv()).await.ok().flatten();
    p.map(|p| p.raw.to_vec())
}

/// bob takes the SUT's INVITE on `cid`, answers it `status`, and its call
/// releases.
async fn bob_answers_then_releases(
    core: &Arc<MuxCore>,
    sut: &dyn UdpEndpoint,
    cid: &str,
    status: &str,
) {
    let net = core.network(CallRouting::new(format!("tok-{cid}")).leg(addr(BOB), "bob"));
    let bob = net.bind_udp(BindUdpOpts::new(addr(BOB), 16)).await.unwrap();
    sut.send_to(&sut_request("INVITE", cid, "1 INVITE"), addr(BOB)).await.unwrap();
    let got = tokio::time::timeout(RECV, bob.recv()).await.unwrap().unwrap();
    assert!(got.raw.starts_with(b"INVITE "));
    let answer = bob_response(status, cid);
    bob.send_to(&answer, addr(SUT)).await.unwrap();
    assert_eq!(recv(sut).await, Some(answer), "bob's live {status}");
}

/// A callee that only rang, released, holds no final for its INVITE: the
/// CANCEL and its retransmission draw the same 481, with a To tag; so does a
/// CANCEL on a Call-ID the mux never saw.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_cancel_of_a_released_ringing_callee_draws_a_481_each_time() {
    let (_sim, core, sut) = bind().await;
    bob_answers_then_releases(&core, sut.as_ref(), "cid-ring", "180 Ringing").await;

    let cancel = sut_request("CANCEL", "cid-ring", "1 CANCEL");
    sut.send_to(&cancel, addr(BOB)).await.unwrap();
    let refused = recv(sut.as_ref()).await.expect("the CANCEL is answered");
    let (req, resp) = (identity(&cancel), identity(&refused));
    assert_eq!(resp.status, Some(481));
    assert_eq!(
        (resp.call_id, resp.cseq, resp.cseq_method, resp.branch),
        (req.call_id, req.cseq, req.cseq_method, req.branch)
    );
    assert!(resp.to_tag.is_some_and(|t| !t.is_empty()), "a UAS response carries a To tag");
    sut.send_to(&cancel, addr(BOB)).await.unwrap();
    assert_eq!(recv(sut.as_ref()).await, Some(refused.clone()), "the retransmission: the same 481");

    let unknown = sut_request("CANCEL", "cid-unknown", "1 CANCEL");
    sut.send_to(&unknown, addr(BOB)).await.unwrap();
    let refused = recv(sut.as_ref()).await.expect("a CANCEL of a call never seen is answered");
    assert_eq!(identity(&refused).status, Some(481));

    let s = core.stats();
    assert_eq!(s.late_answered.load(Relaxed), 3);
    assert_eq!(s.orphans_total(), 0, "no orphan: {:?}", s.samples());
}

/// A callee that rejected its INVITE, released, still holds the 486: the
/// CANCEL has no effect on it and draws a 200 bearing the 486's To tag.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_cancel_of_a_released_rejected_invite_draws_a_200() {
    let (_sim, core, sut) = bind().await;
    bob_answers_then_releases(&core, sut.as_ref(), "cid-busy", "486 Busy Here").await;

    sut.send_to(&sut_request("CANCEL", "cid-busy", "1 CANCEL"), addr(BOB)).await.unwrap();
    let ok = recv(sut.as_ref()).await.expect("the CANCEL is answered");
    let id = identity(&ok);
    assert_eq!((id.status, id.cseq, id.cseq_method), (Some(200), Some(1), Some("CANCEL")));
    assert_eq!(id.to_tag, Some("b1"), "the To tag of the INVITE's final");
    assert_eq!(recv(sut.as_ref()).await, None, "a 200 alone: the INVITE has its final");

    let s = core.stats();
    assert_eq!(s.late_answered.load(Relaxed), 1);
    assert_eq!(s.orphans_total(), 0, "no orphan: {:?}", s.samples());
}
