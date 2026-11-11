//! RFC 3261 §17.1.1.3 at the hard gate: the ACK of a non-2xx final goes to
//! the address, port and transport the INVITE went to. Its severity is the
//! hop's: on a border hop (one end a simulated party) an ACK sent elsewhere
//! fails the run, whichever end owed it; on a hop between two parties of the
//! system under test it is reported and never gates.

use scenario_harness::{report, Harness, RunReport};
use sip_net::UdpEndpoint;

/// Where each party of the transaction stands.
struct Hop {
    /// The UAC: sends the INVITE and owes the ACK.
    uac: Box<dyn UdpEndpoint>,
    uac_addr: std::net::SocketAddr,
    /// The UAS the INVITE goes to; it rejects it with a 486.
    uas: Box<dyn UdpEndpoint>,
    uas_addr: std::net::SocketAddr,
    /// Another socket of the UAS's side, where the UAC sends its ACK.
    elsewhere: Box<dyn UdpEndpoint>,
    elsewhere_addr: std::net::SocketAddr,
}

/// Binds `name` at `addr` as a party under test when `sut`, else as a
/// simulated party.
async fn bind(
    h: &Harness,
    name: &str,
    addr: &str,
    sut: bool,
) -> (Box<dyn UdpEndpoint>, std::net::SocketAddr) {
    if sut {
        h.bind_sut(name, addr).await
    } else {
        h.bind_peer(name, addr).await
    }
}

fn invite(uac: std::net::SocketAddr) -> String {
    format!(
        "INVITE sip:callee@127.0.0.1:5090 SIP/2.0\r\n\
         Via: SIP/2.0/UDP {uac};branch=z9hG4bK-hop-ack\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:caller@127.0.0.1>;tag=uac-1\r\n\
         To: <sip:callee@127.0.0.1>\r\n\
         Call-ID: hop-ack@127.0.0.1\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:caller@{uac}>\r\n\
         Allow: INVITE, ACK, CANCEL, BYE, OPTIONS\r\n\
         Supported: timer\r\n\
         Content-Length: 0\r\n\r\n"
    )
}

fn busy(uac: std::net::SocketAddr) -> String {
    format!(
        "SIP/2.0 486 Busy Here\r\n\
         Via: SIP/2.0/UDP {uac};branch=z9hG4bK-hop-ack\r\n\
         From: <sip:caller@127.0.0.1>;tag=uac-1\r\n\
         To: <sip:callee@127.0.0.1>;tag=uas-1\r\n\
         Call-ID: hop-ack@127.0.0.1\r\n\
         CSeq: 1 INVITE\r\n\
         Content-Length: 0\r\n\r\n"
    )
}

fn ack(uac: std::net::SocketAddr) -> String {
    format!(
        "ACK sip:callee@127.0.0.1:5090 SIP/2.0\r\n\
         Via: SIP/2.0/UDP {uac};branch=z9hG4bK-hop-ack\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:caller@127.0.0.1>;tag=uac-1\r\n\
         To: <sip:callee@127.0.0.1>;tag=uas-1\r\n\
         Call-ID: hop-ack@127.0.0.1\r\n\
         CSeq: 1 ACK\r\n\
         Content-Length: 0\r\n\r\n"
    )
}

/// The UAC invites the UAS, takes its 486 and ACKs it at `elsewhere`, not at
/// the UAS the INVITE went to.
async fn ack_elsewhere(hop: &Hop) {
    hop.uac.send_to(invite(hop.uac_addr).as_bytes(), hop.uas_addr).await.expect("INVITE leaves");
    hop.uas.recv().await.expect("the INVITE reaches the UAS");
    hop.uas.send_to(busy(hop.uac_addr).as_bytes(), hop.uac_addr).await.expect("486 leaves");
    hop.uac.recv().await.expect("the 486 reaches the UAC");
    hop.uac.send_to(ack(hop.uac_addr).as_bytes(), hop.elsewhere_addr).await.expect("ACK leaves");
    hop.elsewhere.recv().await.expect("the ACK reaches the other socket");
}

/// A hop of `uac`, `uas` and `elsewhere`, each under test when its flag says.
async fn hop(h: &Harness, uac: (&str, bool), uas: (&str, bool), elsewhere: (&str, bool)) -> Hop {
    let (uac_ep, uac_addr) = bind(h, uac.0, "127.0.0.1:5060", uac.1).await;
    let (uas_ep, uas_addr) = bind(h, uas.0, "127.0.0.1:5080", uas.1).await;
    let (else_ep, elsewhere_addr) = bind(h, elsewhere.0, "127.0.0.1:5081", elsewhere.1).await;
    Hop { uac: uac_ep, uac_addr, uas: uas_ep, uas_addr, elsewhere: else_ep, elsewhere_addr }
}

/// The report's severity of the run's `unacked-invite-non-2xx-final` finding.
fn unacked_advisory(run: &RunReport) -> Option<bool> {
    let doc = report::seq_doc(run);
    let finding = doc.anomalies.iter().find(|a| a.check == "unacked-invite-non-2xx-final");
    finding
        .unwrap_or_else(|| panic!("an unacked-invite-non-2xx-final finding: {:?}", doc.anomalies))
        .advisory
}

/// A simulated caller ACKs the proxy's reject at another socket of the proxy:
/// a border hop, so the run fails.
#[tokio::test(start_paused = true)]
#[should_panic(
    expected = "red because RFC audit rule unacked-invite-non-2xx-final fired (charged to caller)"
)]
async fn a_simulated_caller_acking_another_proxy_socket_fails_the_run() {
    let h = Harness::new("non-2xx-ack-border-caller");
    let hop = hop(&h, ("caller", false), ("proxy", true), ("proxy-b", true)).await;
    ack_elsewhere(&hop).await;
    h.finish().await;
}

/// The proxy ACKs a simulated callee's reject at another socket than the
/// INVITE's: a border hop, so the run fails.
#[tokio::test(start_paused = true)]
#[should_panic(
    expected = "red because RFC audit rule unacked-invite-non-2xx-final fired (charged to proxy)"
)]
async fn a_proxy_acking_a_simulated_callee_elsewhere_fails_the_run() {
    let h = Harness::new("non-2xx-ack-border-callee");
    let hop = hop(&h, ("proxy", true), ("callee", false), ("callee-b", false)).await;
    ack_elsewhere(&hop).await;
    h.finish().await;
}

/// The proxy ACKs a worker's reject at another worker: both ends of the hop
/// are under test, so the finding is reported as advisory and the run passes.
#[tokio::test(start_paused = true)]
async fn a_proxy_acking_another_worker_is_reported_and_passes() {
    let h = Harness::new("non-2xx-ack-inside-sut");
    let hop = hop(&h, ("proxy", true), ("worker-1", true), ("worker-2", true)).await;
    ack_elsewhere(&hop).await;
    let run = h.finish().await;
    assert_eq!(unacked_advisory(&run), Some(true), "a hop inside the SUT advises");
}
