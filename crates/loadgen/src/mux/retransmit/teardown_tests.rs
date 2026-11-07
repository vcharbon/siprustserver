//! The engine's transaction ends on the paused clock over the simulated
//! fabric: a CANCEL keeps retransmitting until its own final whatever its
//! INVITE draws on the shared branch (RFC 3261 §9.1), a final to the INVITE
//! ends its reliable provisionals (RFC 3262 §3), a retransmitted CANCEL or
//! INVITE draws its own transaction's final again, and an ACKed INVITE final
//! is not re-sent, the hop-ACK of a non-2xx touching no other transaction.

use super::*;
use sip_net::{BindUdpOpts, SignalingNetwork};
use sip_net_testkit::SimulatedSignalingNetwork;
use tokio::time::{timeout, Duration as TokioDuration};

async fn sim_rig() -> (Arc<CallTxns>, Box<dyn UdpEndpoint>, SocketAddr) {
    let net = SimulatedSignalingNetwork::new(1);
    let engine_addr: SocketAddr = "127.0.0.1:5060".parse().unwrap();
    let peer_addr: SocketAddr = "127.0.0.1:5070".parse().unwrap();
    let endpoint: Arc<dyn UdpEndpoint> =
        Arc::from(net.bind_udp(BindUdpOpts::new(engine_addr, 64)).await.unwrap());
    let peer = net.bind_udp(BindUdpOpts::new(peer_addr, 64)).await.unwrap();
    let txns = Arc::new(CallTxns::new(
        endpoint,
        Arc::new(DropModel::new(0.0, 1, None)),
        Arc::new(MuxStats::new(4)),
        None,
    ));
    (txns, peer, peer_addr)
}

/// The first line of every datagram `peer` receives within `window`.
async fn received(peer: &dyn UdpEndpoint, window: TokioDuration) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + window;
    let mut lines = Vec::new();
    while let Ok(Some(pkt)) = timeout(deadline - tokio::time::Instant::now(), peer.recv()).await {
        let text = String::from_utf8_lossy(&pkt.raw).to_string();
        lines.push(text.lines().next().unwrap_or_default().to_string());
    }
    lines
}

fn request(method: &str, cseq: &str) -> Vec<u8> {
    format!(
        "{method} sip:b@127.0.0.1:5070 SIP/2.0\r\nContact: <sip:fixture@192.0.2.99>\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-c1\r\n\
         Call-ID: tc1@h\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>\r\nCSeq: {cseq}\r\n\r\n"
    )
    .into_bytes()
}

fn response(status: u16, cseq: &str, extra: &str) -> Vec<u8> {
    format!(
        "SIP/2.0 {status} X\r\nContact: <sip:fixture@192.0.2.98>\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-c1\r\n\
         Call-ID: tc1@h\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>;tag=2\r\nCSeq: {cseq}\r\n{extra}\r\n"
    )
    .into_bytes()
}

/// A CANCEL shares its INVITE's branch: a reliable 183 retransmitted on that
/// branch after the CANCEL went out ends neither the CANCEL's retransmission
/// nor its transaction; the CANCEL's own 200 ends both.
#[tokio::test(start_paused = true)]
async fn a_cancel_retransmits_until_its_own_final_not_an_invite_provisional() {
    let (txns, peer, peer_addr) = sim_rig().await;
    let r183 = response(183, "1 INVITE", "Require: 100rel\r\nRSeq: 1\r\n");
    txns.on_outbound(&Datagram::sniff(&request("INVITE", "1 INVITE")), peer_addr);
    assert!(txns.on_inbound(&Datagram::sniff(&r183), peer_addr));
    txns.on_outbound(&Datagram::sniff(&request("CANCEL", "1 CANCEL")), peer_addr);
    assert!(
        !txns.on_inbound(&Datagram::sniff(&r183), peer_addr),
        "the retransmitted 183 is absorbed"
    );

    let sent = received(peer.as_ref(), TokioDuration::from_millis(1_200)).await;
    assert!(
        sent.iter().any(|l| l.starts_with("CANCEL ")),
        "the CANCEL is retransmitted (Timer E) past the 183: {sent:?}"
    );
    let ended = txns.clone();
    let ended = tokio::spawn(async move { ended.clients_ended().await });
    tokio::time::sleep(TokioDuration::from_millis(10)).await;
    assert!(!ended.is_finished(), "the CANCEL's transaction is still open");

    assert!(txns.on_inbound(&Datagram::sniff(&response(200, "1 CANCEL", "")), peer_addr));
    timeout(TokioDuration::from_millis(10), ended)
        .await
        .expect("the CANCEL's 200 ends its transaction")
        .unwrap();
    let after = received(peer.as_ref(), TokioDuration::from_secs(5)).await;
    assert!(after.iter().all(|l| !l.starts_with("CANCEL ")), "no CANCEL after its 200: {after:?}");
    txns.shutdown();
}

/// A client transaction no final answers ends at Timer F: the wait for it is
/// bounded by 64·T1 after the request.
#[tokio::test(start_paused = true)]
async fn an_unanswered_request_ends_at_timer_f() {
    let (txns, _peer, peer_addr) = sim_rig().await;
    let start = tokio::time::Instant::now();
    txns.on_outbound(&Datagram::sniff(&request("CANCEL", "1 CANCEL")), peer_addr);
    txns.clients_ended().await;
    assert_eq!(start.elapsed(), TXN_TIMEOUT, "the CANCEL's transaction ends at Timer F");
    txns.shutdown();
}

/// A final to the INVITE ends its reliable provisional: the 183 is not
/// retransmitted after the 504 that gives up on it.
#[tokio::test(start_paused = true)]
async fn an_invite_final_stops_its_reliable_provisional() {
    let (txns, peer, peer_addr) = sim_rig().await;
    txns.on_outbound(
        &Datagram::sniff(&response(183, "1 INVITE", "Require: 100rel\r\nRSeq: 1\r\n")),
        peer_addr,
    );
    txns.on_outbound(&Datagram::sniff(&response(504, "1 INVITE", "")), peer_addr);
    let sent = received(peer.as_ref(), TokioDuration::from_secs(5)).await;
    assert!(sent.iter().all(|l| !l.contains(" 183 ")), "no 183 after the final: {sent:?}");
    assert!(sent.iter().any(|l| l.contains(" 504 ")), "the 504 is retransmitted: {sent:?}");
    txns.shutdown();
}

/// An ACK of an INVITE's final, on `branch`.
fn ack(branch: &str) -> Vec<u8> {
    format!(
        "ACK sip:b@127.0.0.1:5070 SIP/2.0\r\nMax-Forwards: 70\r\nVia: SIP/2.0/UDP 127.0.0.1:5070;branch={branch}\r\n\
         Call-ID: tc1@h\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>;tag=2\r\nCSeq: 1 ACK\r\n\r\n"
    )
    .into_bytes()
}

/// A CANCEL and its INVITE are two server transactions on one branch
/// (RFC 3261 §9.1): a retransmitted CANCEL draws the CANCEL's 200 again, a
/// retransmitted INVITE the INVITE's 487.
#[tokio::test(start_paused = true)]
async fn a_retransmitted_cancel_and_invite_each_draw_their_own_final() {
    let (txns, peer, peer_addr) = sim_rig().await;
    let (invite, cancel) = (request("INVITE", "1 INVITE"), request("CANCEL", "1 CANCEL"));
    assert!(txns.on_inbound(&Datagram::sniff(&invite), peer_addr));
    assert!(txns.on_inbound(&Datagram::sniff(&cancel), peer_addr));
    txns.on_outbound(&Datagram::sniff(&response(200, "1 CANCEL", "")), peer_addr);
    txns.on_outbound(&Datagram::sniff(&response(487, "1 INVITE", "")), peer_addr);
    // Well inside T1: the 487's own Timer G resend is not due.
    let window = TokioDuration::from_millis(100);
    assert!(!txns.on_inbound(&Datagram::sniff(&cancel), peer_addr), "the CANCEL absorbed");
    assert_eq!(received(peer.as_ref(), window).await, ["SIP/2.0 200 X"], "its own 200");
    assert!(!txns.on_inbound(&Datagram::sniff(&invite), peer_addr), "the INVITE absorbed");
    assert_eq!(received(peer.as_ref(), window).await, ["SIP/2.0 487 X"], "its own 487");
    txns.shutdown();
}

/// Once its ACK arrived, an INVITE's final is owed to no retransmitted
/// INVITE: the retransmission is absorbed and draws nothing (RFC 3261
/// §17.2.1 Confirmed for a non-2xx; RFC 6026 Accepted for a 2xx, whose ACK
/// is a transaction of its own).
#[tokio::test(start_paused = true)]
async fn an_acked_invite_final_is_not_sent_again() {
    for (status, ack_branch) in [(486, "z9hG4bK-c1"), (200, "z9hG4bK-a2")] {
        let (txns, peer, peer_addr) = sim_rig().await;
        let invite = request("INVITE", "1 INVITE");
        assert!(txns.on_inbound(&Datagram::sniff(&invite), peer_addr));
        txns.on_outbound(&Datagram::sniff(&response(status, "1 INVITE", "")), peer_addr);
        assert!(txns.on_inbound(&Datagram::sniff(&ack(ack_branch)), peer_addr), "its ACK");
        assert!(!txns.on_inbound(&Datagram::sniff(&invite), peer_addr), "{status}: absorbed");
        let resent = received(peer.as_ref(), TokioDuration::from_secs(40)).await;
        assert!(resent.is_empty(), "{status}: nothing sent after the ACK: {resent:?}");
        txns.shutdown();
    }
}

/// `raw` on top Via branch `branch`.
fn on_branch(raw: Vec<u8>, branch: &str) -> Vec<u8> {
    String::from_utf8(raw).unwrap().replace("z9hG4bK-c1", branch).into_bytes()
}

/// A merged request (RFC 3261 §8.2.2.2): one INVITE reaches the engine on two
/// branches, answered 200 on the first and 482 on the second. The 482's
/// hop-ACK, on the second branch (§17.1.1.3), ends that transaction only: the
/// 200 is still retransmitted until its own ACK, and still re-answers a
/// retransmission of the first branch's INVITE.
#[tokio::test(start_paused = true)]
async fn a_merged_requests_hop_ack_leaves_the_other_branchs_2xx() {
    let (txns, peer, peer_addr) = sim_rig().await;
    let (b1, b2) = ("z9hG4bK-m1", "z9hG4bK-m2");
    let invite = |b| on_branch(request("INVITE", "1 INVITE"), b);
    assert!(txns.on_inbound(&Datagram::sniff(&invite(b1)), peer_addr));
    txns.on_outbound(&Datagram::sniff(&on_branch(response(200, "1 INVITE", ""), b1)), peer_addr);
    assert!(txns.on_inbound(&Datagram::sniff(&invite(b2)), peer_addr), "another transaction");
    txns.on_outbound(&Datagram::sniff(&on_branch(response(482, "1 INVITE", ""), b2)), peer_addr);
    assert!(txns.on_inbound(&Datagram::sniff(&ack(b2)), peer_addr), "the 482's hop-ACK");
    assert!(!txns.on_inbound(&Datagram::sniff(&invite(b1)), peer_addr), "absorbed");
    let window = TokioDuration::from_millis(100);
    assert_eq!(received(peer.as_ref(), window).await, ["SIP/2.0 200 X"], "re-answered");
    let resent = received(peer.as_ref(), TokioDuration::from_secs(2)).await;
    assert!(!resent.is_empty() && resent.iter().all(|l| l == "SIP/2.0 200 X"), "{resent:?}");
    txns.shutdown();
}
