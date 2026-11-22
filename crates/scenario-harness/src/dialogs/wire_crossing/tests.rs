//! The BYE-crossing fact, read from a recording of a simulated fabric: a
//! peer `S` (192.0.2.1) sends its BYE (CSeq 7) to the UA `U` (192.0.2.2),
//! and `U` sends its own BYE numbered 3 to `S`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use layer_harness::{Recorder, RunContext};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};
use sip_net::{BindUdpOpts, UdpEndpoint};
use sip_net_testkit::{with_all_contracts, Fate, ScopedAuditOptions, SimulatedSignalingNetwork};

use super::SentBeforeBye;

const PEERS_BYE: &str =
    "BYE sip:u@192.0.2.2 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-s7\r\n\
     From: <sip:s@host>;tag=S\r\nTo: <sip:u@host>;tag=U\r\nCall-ID: c1@host\r\nCSeq: 7 BYE\r\n\
     Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
const UAS_BYE: &str =
    "BYE sip:s@192.0.2.1 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.2:5060;branch=z9hG4bK-u3\r\n\
     From: <sip:u@host>;tag=U\r\nTo: <sip:s@host>;tag=S\r\nCall-ID: c1@host\r\nCSeq: 3 BYE\r\n\
     Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n";

fn peers_bye() -> SipRequest {
    match CustomParser::new().parse(PEERS_BYE.as_bytes()).expect("a well-formed BYE") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => unreachable!(),
    }
}

/// The fact of the peer's BYE and the UA's BYE numbered `bye_cseq`, on a
/// fabric that loses the peer's first datagram when `lose`, after the sends
/// `script` makes: `true` a copy of the peer's BYE, `false` the UA's BYE,
/// each delivered before the next.
async fn fact_after(lose: bool, script: &[bool], bye_cseq: u32) -> bool {
    let peer: SocketAddr = "192.0.2.1:5060".parse().unwrap();
    let first = AtomicBool::new(lose);
    let fabric = SimulatedSignalingNetwork::new(1).with_send_fate(Arc::new(move |src, _, _| {
        if src == peer && first.swap(false, Ordering::SeqCst) {
            Fate::Lose
        } else {
            Fate::Deliver
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
    let s = net.bind_udp(BindUdpOpts::new(peer, 64)).await.unwrap();
    let u = net.bind_udp(BindUdpOpts::new("192.0.2.2:5060".parse().unwrap(), 64)).await.unwrap();
    for &from_peer in script {
        let (from, to, raw): (&dyn UdpEndpoint, _, _) = match from_peer {
            true => (s.as_ref(), u.local_addr(), PEERS_BYE),
            false => (u.as_ref(), s.local_addr(), UAS_BYE),
        };
        from.send_to(raw.as_bytes(), to).await.unwrap();
        net.await_in_flight(Duration::from_secs(1)).await;
    }
    SentBeforeBye::from_wire(wrapped.recording.channel()).holds(&peers_bye(), bye_cseq)
}

/// The peer's first copy, lost, left before the UA's BYE reached it: its
/// retransmission crossed that BYE.
#[tokio::test(start_paused = true)]
async fn a_lost_first_copy_sent_before_the_uas_bye_arrived_crossed_it() {
    assert!(fact_after(true, &[true, false, true], 3).await);
}

/// The peer's first copy was delivered: what reached the UA later is no
/// crossing of a lost copy.
#[tokio::test(start_paused = true)]
async fn a_delivered_first_copy_crossed_nothing() {
    assert!(!fact_after(false, &[true, false, true], 3).await);
}

/// The UA's BYE reached the peer before its first copy left: the peer sent
/// it knowing the dialog ended.
#[tokio::test(start_paused = true)]
async fn a_first_copy_sent_after_the_uas_bye_arrived_crossed_nothing() {
    assert!(!fact_after(true, &[false, true, true], 3).await);
}

/// Only the arrival of the BYE that ended the dialog counts: the UA's BYE
/// numbered 3 says nothing of one numbered 4, which never reached the peer.
#[tokio::test(start_paused = true)]
async fn only_the_bye_that_ended_the_dialog_counts() {
    assert!(fact_after(true, &[false, true, true], 4).await);
}
