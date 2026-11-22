//! The ahead-of-final fact, read from a recording of a simulated fabric: the
//! UA `U` (192.0.2.2) sends the peer `S` (192.0.2.1) copies of its INFO
//! (CSeq 1) on their early dialog, and the dialog ends by `U`'s own 487 to
//! `S`'s INVITE numbered 1 or by `S`'s own 487 to `U`'s; the fabric loses the
//! copies of `U`'s a scenario names.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use layer_harness::{Recorder, RunContext};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipResponse};
use sip_net::BindUdpOpts;
use sip_net_testkit::{with_all_contracts, Fate, ScopedAuditOptions, SimulatedSignalingNetwork};

use super::{AheadOfFinal, DialogEnd, Ending};

const UA_INFO: &str =
    "INFO sip:s@192.0.2.1 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.2:5060;branch=z9hG4bK-u1\r\n\
     From: <sip:u@host>;tag=U\r\nTo: <sip:s@host>;tag=S\r\nCall-ID: c1@host\r\nCSeq: 1 INFO\r\n\
     Contact: <sip:u@192.0.2.2>\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
const UAS_487: &str = "SIP/2.0 487 Request Terminated\r\n\
     Via: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-s1\r\n\
     From: <sip:s@host>;tag=S\r\nTo: <sip:u@host>;tag=U\r\nCall-ID: c1@host\r\n\
     CSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
const PEERS_487: &str = "SIP/2.0 487 Request Terminated\r\n\
     Via: SIP/2.0/UDP 192.0.2.2:5060;branch=z9hG4bK-u0\r\n\
     From: <sip:u@host>;tag=U\r\nTo: <sip:s@host>;tag=S\r\nCall-ID: c1@host\r\n\
     CSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
const PEERS_200: &str = "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 192.0.2.2:5060;branch=z9hG4bK-u1\r\n\
     From: <sip:u@host>;tag=U\r\nTo: <sip:s@host>;tag=S\r\nCall-ID: c1@host\r\nCSeq: 1 INFO\r\n\
     Content-Length: 0\r\n\r\n";

/// The UA's own 487, which ends the dialog when it arrives at the peer.
const OWN_487: DialogEnd =
    DialogEnd { ending: Ending::Final { invite_cseq: 1, status: 487 }, by_peer: false };
/// The peer's own 487, which ends the dialog when the peer sends it.
const PEERS_OWN_487: DialogEnd = DialogEnd { by_peer: true, ..OWN_487 };

/// One datagram of the scenario, each delivered or lost before the next.
#[derive(Clone, Copy)]
enum Out {
    /// A copy of the UA's INFO.
    Info,
    /// A copy of the UA's own 487.
    Final,
    /// The peer's own 487.
    PeerFinal,
}

use Out::{Final, Info, PeerFinal};

fn peers_200() -> SipResponse {
    match CustomParser::new().parse(PEERS_200.as_bytes()).expect("a well-formed 200") {
        SipMessage::Response(r) => r,
        SipMessage::Request(_) => unreachable!(),
    }
}

/// The fact of the peer's 200 to the INFO and the dialog's `end`, after
/// `sends`, the fabric losing the UA's datagrams at the indexes `lost`.
async fn fact_after(sends: &[Out], lost: &'static [usize], end: DialogEnd) -> Option<bool> {
    let ua: SocketAddr = "192.0.2.2:5060".parse().unwrap();
    let lost: BTreeSet<usize> = lost.iter().copied().collect();
    let sent = AtomicUsize::new(0);
    let fabric = SimulatedSignalingNetwork::new(1).with_send_fate(Arc::new(move |src, _, _| {
        match src == ua && lost.contains(&sent.fetch_add(1, Ordering::SeqCst)) {
            true => Fate::Lose,
            false => Fate::Deliver,
        }
    }));
    let wrapped = with_all_contracts(
        Arc::new(fabric),
        Recorder::fake(),
        RunContext::TestWithRecorder,
        ScopedAuditOptions::default(),
        true,
    );
    let net = wrapped.network.clone();
    let s = net.bind_udp(BindUdpOpts::new("192.0.2.1:5060".parse().unwrap(), 64)).await.unwrap();
    let u = net.bind_udp(BindUdpOpts::new(ua, 64)).await.unwrap();
    for send in sends {
        match send {
            Info => u.send_to(UA_INFO.as_bytes(), s.local_addr()).await.unwrap(),
            Final => u.send_to(UAS_487.as_bytes(), s.local_addr()).await.unwrap(),
            PeerFinal => s.send_to(PEERS_487.as_bytes(), u.local_addr()).await.unwrap(),
        }
        net.await_in_flight(Duration::from_secs(1)).await;
    }
    AheadOfFinal::from_wire(wrapped.recording.channel()).holds(&peers_200(), end)
}

/// The INFO's first copy was delivered before the 487 left: it reached the
/// peer on the live dialog.
#[tokio::test(start_paused = true)]
async fn a_delivered_copy_sent_before_the_final_reached_ahead() {
    assert_eq!(fact_after(&[Info, Final], &[], OWN_487).await, Some(true));
}

/// The INFO's first copy lost, a retransmission sent before the 487 reached
/// the peer ahead of it.
#[tokio::test(start_paused = true)]
async fn a_retransmission_sent_before_the_final_reached_ahead() {
    assert_eq!(fact_after(&[Info, Info, Final], &[0], OWN_487).await, Some(true));
}

/// The INFO's only copy sent before the 487 lost, its retransmission after
/// it reached the peer behind that final; with no copy delivered, the record
/// cannot tell.
#[tokio::test(start_paused = true)]
async fn a_request_whose_copies_before_the_final_were_lost_did_not_reach_ahead() {
    assert_eq!(fact_after(&[Info, Final, Info], &[0], OWN_487).await, Some(false));
    assert_eq!(fact_after(&[Info, Final], &[0], OWN_487).await, None);
}

/// The 487's first copy lost, an INFO sent after it reached the peer ahead of
/// the 487's delivered retransmission; one sent after the delivered 487 did
/// not.
#[tokio::test(start_paused = true)]
async fn the_final_counts_from_its_first_delivered_copy() {
    assert_eq!(fact_after(&[Final, Info, Final], &[0], OWN_487).await, Some(true));
    assert_eq!(fact_after(&[Final, Info], &[], OWN_487).await, Some(false));
}

/// The peer's own 487 ends the dialog when the peer sends it: an INFO that
/// arrived before met the live dialog, one that arrived after, its first copy
/// lost or sent after, did not.
#[tokio::test(start_paused = true)]
async fn the_peers_own_final_counts_from_its_send() {
    assert_eq!(fact_after(&[Info, PeerFinal], &[], PEERS_OWN_487).await, Some(true));
    assert_eq!(fact_after(&[Info, PeerFinal, Info], &[0], PEERS_OWN_487).await, Some(false));
    assert_eq!(fact_after(&[PeerFinal, Info], &[], PEERS_OWN_487).await, Some(false));
}

/// The INFO's first copy lost, the 487 held 1 s by the fabric, the INFO's
/// retransmission sent 500 ms later overtakes it: the peer met the INFO first,
/// on the live dialog. Arrival at the peer counts, not the UA's send order.
#[tokio::test(start_paused = true)]
async fn a_retransmission_overtaking_a_held_final_reached_ahead() {
    let ua: SocketAddr = "192.0.2.2:5060".parse().unwrap();
    let sent = AtomicUsize::new(0);
    let fabric = SimulatedSignalingNetwork::new(1).with_send_fate(Arc::new(move |src, _, _| {
        if src != ua {
            return Fate::Deliver;
        }
        match sent.fetch_add(1, Ordering::SeqCst) {
            0 => Fate::Lose,
            1 => Fate::Late(Duration::from_millis(1000)),
            _ => Fate::Deliver,
        }
    }));
    let wrapped = with_all_contracts(
        Arc::new(fabric),
        Recorder::fake(),
        RunContext::TestWithRecorder,
        ScopedAuditOptions::default(),
        true,
    );
    let net = wrapped.network.clone();
    let s = net.bind_udp(BindUdpOpts::new("192.0.2.1:5060".parse().unwrap(), 64)).await.unwrap();
    let u = net.bind_udp(BindUdpOpts::new(ua, 64)).await.unwrap();
    u.send_to(UA_INFO.as_bytes(), s.local_addr()).await.unwrap();
    u.send_to(UAS_487.as_bytes(), s.local_addr()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    u.send_to(UA_INFO.as_bytes(), s.local_addr()).await.unwrap();
    let arrived: Vec<String> = [s.recv().await, s.recv().await]
        .iter()
        .flatten()
        .map(|p| String::from_utf8_lossy(&p.raw).lines().next().unwrap_or_default().to_string())
        .collect();
    assert_eq!(arrived, ["INFO sip:s@192.0.2.1 SIP/2.0", "SIP/2.0 487 Request Terminated"]);
    let fact = AheadOfFinal::from_wire(wrapped.recording.channel()).holds(&peers_200(), OWN_487);
    assert_eq!(fact, Some(true));
}
