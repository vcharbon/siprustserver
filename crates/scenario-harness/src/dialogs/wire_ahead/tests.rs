//! The ahead-of-final fact, read from a recording of a simulated fabric: the
//! UA `U` (192.0.2.2) sends the peer `S` (192.0.2.1) copies of its INFO
//! (CSeq 1) on their early dialog and of its own 487 to `S`'s INVITE numbered
//! 1, the fabric losing the copies a scenario names.

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

use super::AheadOfFinal;

const UAS_INFO: &str =
    "INFO sip:s@192.0.2.1 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.2:5060;branch=z9hG4bK-u1\r\n\
     From: <sip:u@host>;tag=U\r\nTo: <sip:s@host>;tag=S\r\nCall-ID: c1@host\r\nCSeq: 1 INFO\r\n\
     Contact: <sip:u@192.0.2.2>\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
const UAS_487: &str = "SIP/2.0 487 Request Terminated\r\n\
     Via: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-s1\r\n\
     From: <sip:s@host>;tag=S\r\nTo: <sip:u@host>;tag=U\r\nCall-ID: c1@host\r\n\
     CSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
const PEERS_200: &str = "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 192.0.2.2:5060;branch=z9hG4bK-u1\r\n\
     From: <sip:u@host>;tag=U\r\nTo: <sip:s@host>;tag=S\r\nCall-ID: c1@host\r\nCSeq: 1 INFO\r\n\
     Content-Length: 0\r\n\r\n";

/// One datagram the UA sends, each delivered or lost before the next.
#[derive(Clone, Copy)]
enum Out {
    /// A copy of its INFO.
    Info,
    /// A copy of its 487.
    Final,
}

use Out::{Final, Info};

fn peers_200() -> SipResponse {
    match CustomParser::new().parse(PEERS_200.as_bytes()).expect("a well-formed 200") {
        SipMessage::Response(r) => r,
        SipMessage::Request(_) => unreachable!(),
    }
}

/// The fact of the peer's 200 to the INFO and the UA's 487, after the UA
/// sent `sends`, the fabric losing the ones at the indexes `lost`.
async fn fact_after(sends: &[Out], lost: &'static [usize]) -> bool {
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
        let text = match send {
            Info => UAS_INFO,
            Final => UAS_487,
        };
        u.send_to(text.as_bytes(), s.local_addr()).await.unwrap();
        net.await_in_flight(Duration::from_secs(1)).await;
    }
    AheadOfFinal::from_wire(wrapped.recording.channel()).holds(&peers_200(), 1, 487)
}

/// The INFO's first copy was delivered before the 487 left: it reached the
/// peer on the live dialog.
#[tokio::test(start_paused = true)]
async fn a_delivered_copy_sent_before_the_final_reached_ahead() {
    assert!(fact_after(&[Info, Final], &[]).await);
}

/// The INFO's first copy lost, a retransmission sent before the 487 reached
/// the peer ahead of it.
#[tokio::test(start_paused = true)]
async fn a_retransmission_sent_before_the_final_reached_ahead() {
    assert!(fact_after(&[Info, Info, Final], &[0]).await);
}

/// The INFO's only copy sent before the 487 lost, its retransmission after
/// it reached the peer behind that final; a copy never delivered reached
/// nothing.
#[tokio::test(start_paused = true)]
async fn a_request_whose_copies_before_the_final_were_lost_did_not_reach_ahead() {
    assert!(!fact_after(&[Info, Final, Info], &[0]).await);
    assert!(!fact_after(&[Info, Final], &[0]).await);
}

/// The 487's first copy lost, an INFO sent after it reached the peer ahead of
/// the 487's delivered retransmission; one sent after the delivered 487 did
/// not.
#[tokio::test(start_paused = true)]
async fn the_final_counts_from_its_first_delivered_copy() {
    assert!(fact_after(&[Final, Info, Final], &[0]).await);
    assert!(!fact_after(&[Final, Info], &[]).await);
}
