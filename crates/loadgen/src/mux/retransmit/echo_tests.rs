//! The echo peer: its message goes out again on every reaction to it, and an
//! answerer that answers every echo is an overrun at the cap.

use super::echo::ECHO_CAP;
use super::*;
use sip_net::{BindUdpOpts, RealSignalingNetwork, SignalingNetwork};
use tokio::net::UdpSocket as TokioUdp;
use tokio::time::{timeout, Duration as TokioDuration};

async fn echo_rig() -> (CallTxns, Arc<MuxStats>, TokioUdp, SocketAddr) {
    let endpoint: Arc<dyn UdpEndpoint> = Arc::from(
        RealSignalingNetwork::new()
            .bind_udp(BindUdpOpts::new("127.0.0.1:0".parse().unwrap(), 256))
            .await
            .unwrap(),
    );
    let peer = TokioUdp::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let stats = Arc::new(MuxStats::new(4));
    let drop = Arc::new(DropModel::new(0.0, 1, None));
    let txns = CallTxns::new(endpoint, drop, stats.clone(), None, Some(Duration::ZERO));
    (txns, stats, peer, peer_addr)
}

/// The datagrams `peer` receives within `window_ms` of the last one.
async fn received(peer: &TokioUdp, window_ms: u64) -> Vec<String> {
    let mut buf = vec![0u8; 2048];
    let mut got = Vec::new();
    while let Ok(Ok((n, _))) =
        timeout(TokioDuration::from_millis(window_ms), peer.recv_from(&mut buf)).await
    {
        got.push(String::from_utf8_lossy(&buf[..n]).to_string());
    }
    got
}

fn message(start: &str, branch: &str, cseq: &str) -> Vec<u8> {
    format!(
        "{start}\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch={branch}\r\nMax-Forwards: 70\r\n\
         Call-ID: echo1@h\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>;tag=2\r\nCSeq: {cseq}\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "slow lane: loadgen"]
async fn a_request_is_echoed_on_each_final_and_an_answerer_answering_every_echo_overruns() {
    let (txns, stats, peer, peer_addr) = echo_rig().await;
    let update = message("UPDATE sip:b@127.0.0.1 SIP/2.0", "z9hG4bK-e1", "2 UPDATE");
    txns.on_outbound(&Datagram::sniff(&update), peer_addr);
    let ok = message("SIP/2.0 200 OK", "z9hG4bK-e1", "2 UPDATE");
    for _ in 0..ECHO_CAP + 5 {
        txns.on_inbound(&Datagram::sniff(&ok), peer_addr);
    }
    let echoes = received(&peer, 300).await;
    assert_eq!(echoes.len(), ECHO_CAP as usize, "one echo per final up to the cap");
    assert!(echoes.iter().all(|m| m.starts_with("UPDATE ")), "each echo is the request");
    assert_eq!(stats.echo_overruns.load(Ordering::Relaxed), 1, "one overrun past the cap");
    assert_eq!(stats.echo_overrun_calls(), vec!["echo1@h".to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "slow lane: loadgen"]
async fn a_final_to_an_invite_is_echoed_on_its_ack_and_a_bounded_answerer_is_no_overrun() {
    let (txns, stats, peer, peer_addr) = echo_rig().await;
    let invite_ok = message("SIP/2.0 200 OK", "z9hG4bK-i1", "1 INVITE");
    txns.on_outbound(&Datagram::sniff(&invite_ok), peer_addr);
    let ack = message("ACK sip:b@127.0.0.1 SIP/2.0", "z9hG4bK-a1", "1 ACK");
    txns.on_inbound(&Datagram::sniff(&ack), peer_addr);
    let echoes = received(&peer, 200).await;
    assert_eq!(echoes.len(), 1, "the ACK draws the 2xx once: {echoes:?}");
    assert!(echoes[0].starts_with("SIP/2.0 200"), "the echo is the 2xx");
    assert_eq!(stats.echo_overruns.load(Ordering::Relaxed), 0);
}
