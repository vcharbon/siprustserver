//! The crossing fact, read from a recording of a simulated fabric: a peer `S`
//! (192.0.2.1) sends its UPDATE (CSeq 7) to the UA `U` (192.0.2.2), the
//! fabric losing its first copy; the dialog ends by `U`'s BYE numbered 3, or
//! by `S`'s own 487 to `U`'s INVITE numbered 1 or its own BYE numbered 8.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use layer_harness::{Recorder, RunContext};
use sip_message::parser::custom::CustomParser;
use sip_message::{sniff, SipMessage, SipParser, SipRequest};
use sip_net::BindUdpOpts;
use sip_net_testkit::{with_all_contracts, Fate, ScopedAuditOptions, SimulatedSignalingNetwork};

use super::{DialogEnd, Ending, SentBeforeEnd};

const PEERS_UPDATE: &str =
    "UPDATE sip:u@192.0.2.2 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-s7\r\n\
     From: <sip:s@host>;tag=S\r\nTo: <sip:u@host>;tag=U\r\nCall-ID: c1@host\r\nCSeq: 7 UPDATE\r\n\
     Contact: <sip:s@192.0.2.1>\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
const UAS_BYE: &str =
    "BYE sip:s@192.0.2.1 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.2:5060;branch=z9hG4bK-u3\r\n\
     From: <sip:u@host>;tag=U\r\nTo: <sip:s@host>;tag=S\r\nCall-ID: c1@host\r\nCSeq: 3 BYE\r\n\
     Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n";
const PEERS_487: &str = "SIP/2.0 487 Request Terminated\r\n\
     Via: SIP/2.0/UDP 192.0.2.2:5060;branch=z9hG4bK-u1\r\n\
     From: <sip:u@host>;tag=U\r\nTo: <sip:s@host>;tag=S\r\nCall-ID: c1@host\r\n\
     CSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
const PEERS_BYE: &str =
    "BYE sip:u@192.0.2.2 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-s8\r\n\
     From: <sip:s@host>;tag=S\r\nTo: <sip:u@host>;tag=U\r\nCall-ID: c1@host\r\nCSeq: 8 BYE\r\n\
     Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n";

/// The UA's BYE numbered `cseq`.
fn uas_bye(cseq: u32) -> DialogEnd {
    DialogEnd { ending: Ending::Bye { cseq }, by_peer: false }
}

/// The peer's own BYE numbered `cseq`.
fn peers_bye(cseq: u32) -> DialogEnd {
    DialogEnd { ending: Ending::Bye { cseq }, by_peer: true }
}

/// The peer's own 487 to the INVITE numbered `invite_cseq`.
fn peers_487(invite_cseq: u32) -> DialogEnd {
    DialogEnd { ending: Ending::Final { invite_cseq, status: 487 }, by_peer: true }
}

fn peers_update() -> SipRequest {
    match CustomParser::new().parse(PEERS_UPDATE.as_bytes()).expect("a well-formed UPDATE") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => unreachable!(),
    }
}

/// One step of a scenario, each delivered before the next.
#[derive(Clone, Copy)]
enum Step {
    /// The peer sends a copy of its UPDATE.
    PeerCopy,
    /// The UA sends its BYE.
    UasBye,
    /// The peer sends this message of its own.
    PeerSends(&'static str),
    /// The peer takes what its socket holds.
    PeerReads,
    /// The clock moves on a millisecond.
    Tick,
}

/// The fact of the peer's UPDATE and the `end` of its dialog, on a fabric
/// that loses the peer's first UPDATE when `lose`, after `steps`.
async fn fact_after(lose: bool, steps: &[Step], end: DialogEnd) -> bool {
    let peer: SocketAddr = "192.0.2.1:5060".parse().unwrap();
    let first = AtomicBool::new(lose);
    let fabric = SimulatedSignalingNetwork::new(1).with_send_fate(Arc::new(move |src, _, raw| {
        let update = sniff::req_method(raw).as_deref() == Some("UPDATE");
        if src == peer && update && first.swap(false, Ordering::SeqCst) {
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
    for step in steps {
        match step {
            Step::PeerCopy => s.send_to(PEERS_UPDATE.as_bytes(), u.local_addr()).await.unwrap(),
            Step::UasBye => u.send_to(UAS_BYE.as_bytes(), s.local_addr()).await.unwrap(),
            Step::PeerSends(text) => s.send_to(text.as_bytes(), u.local_addr()).await.unwrap(),
            Step::PeerReads => while s.try_recv().is_some() {},
            Step::Tick => tokio::time::advance(Duration::from_millis(1)).await,
        }
        net.await_in_flight(Duration::from_secs(1)).await;
    }
    SentBeforeEnd::from_wire(wrapped.recording.channel()).holds(&peers_update(), end)
}

use Step::{PeerCopy, PeerReads, PeerSends, Tick, UasBye};

/// The peer's first copy, lost, left before it took the UA's BYE: its
/// retransmission crossed that BYE.
#[tokio::test(start_paused = true)]
async fn a_lost_first_copy_sent_before_the_uas_bye_was_received_crossed_it() {
    assert!(fact_after(true, &[PeerCopy, UasBye, PeerReads, PeerCopy], uas_bye(3)).await);
}

/// A BYE that reached the peer's socket but was not read yet is not
/// received: a first copy sent then crossed it.
#[tokio::test(start_paused = true)]
async fn a_bye_waiting_unread_is_not_received() {
    assert!(fact_after(true, &[UasBye, PeerCopy, PeerReads, PeerCopy], uas_bye(3)).await);
}

/// The peer's first copy was delivered: what reached the UA later is no
/// crossing of a lost copy.
#[tokio::test(start_paused = true)]
async fn a_delivered_first_copy_crossed_nothing() {
    assert!(!fact_after(false, &[PeerCopy, UasBye, PeerReads, PeerCopy], uas_bye(3)).await);
}

/// The peer took the UA's BYE at an earlier instant than its first copy
/// left: it sent it knowing the dialog ended.
#[tokio::test(start_paused = true)]
async fn a_first_copy_sent_after_the_uas_bye_was_received_crossed_nothing() {
    assert!(!fact_after(true, &[UasBye, PeerReads, Tick, PeerCopy, PeerCopy], uas_bye(3)).await);
}

/// The peer took the UA's BYE at the instant its first copy left: two
/// events of one instant have no order, so the copy crossed that BYE.
#[tokio::test(start_paused = true)]
async fn a_first_copy_sent_as_the_uas_bye_was_received_crossed_it() {
    assert!(fact_after(true, &[UasBye, PeerReads, PeerCopy, Tick, PeerCopy], uas_bye(3)).await);
}

/// Only the BYE that ended the dialog counts: the UA's BYE numbered 3 says
/// nothing of one numbered 4, which never reached the peer.
#[tokio::test(start_paused = true)]
async fn only_the_bye_that_ended_the_dialog_counts() {
    assert!(fact_after(true, &[UasBye, PeerReads, Tick, PeerCopy, PeerCopy], uas_bye(4)).await);
}

/// The peer's first copy, lost, left before the peer sent its own final or
/// BYE: its retransmission crossed that end.
#[tokio::test(start_paused = true)]
async fn a_lost_first_copy_sent_before_the_peers_own_end_crossed_it() {
    let steps = [PeerCopy, PeerSends(PEERS_487), PeerCopy];
    assert!(fact_after(true, &steps, peers_487(1)).await);
    let steps = [PeerCopy, PeerSends(PEERS_BYE), PeerCopy];
    assert!(fact_after(true, &steps, peers_bye(8)).await);
}

/// The peer sent its own final before its first copy: it sent that copy
/// knowing the dialog ended. Delivered, the first copy crossed nothing.
#[tokio::test(start_paused = true)]
async fn a_first_copy_sent_after_the_peers_own_end_crossed_nothing() {
    let steps = [PeerSends(PEERS_487), PeerCopy, PeerCopy];
    assert!(!fact_after(true, &steps, peers_487(1)).await);
    let steps = [PeerCopy, PeerSends(PEERS_487), PeerCopy];
    assert!(!fact_after(false, &steps, peers_487(1)).await);
}

/// An end of the peer's the lane never shows is no end it sent after its
/// first copy: a final to another INVITE, a BYE of another number.
#[tokio::test(start_paused = true)]
async fn only_the_peers_end_that_ended_the_dialog_counts() {
    let steps = [PeerCopy, PeerSends(PEERS_487), PeerSends(PEERS_BYE), PeerCopy];
    assert!(!fact_after(true, &steps, peers_487(2)).await);
    assert!(!fact_after(true, &steps, peers_bye(9)).await);
}
