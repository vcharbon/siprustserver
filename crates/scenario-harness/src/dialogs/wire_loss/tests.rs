//! The lost-request fact, read from a recording of a simulated fabric that
//! loses the first datagram the peer `S` (192.0.2.1) sends the UA `U`
//! (192.0.2.2): requests of the dialog `S`/`U` and of others.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use layer_harness::{Recorder, RunContext};
use sip_net::BindUdpOpts;
use sip_net_testkit::{with_all_contracts, Fate, ScopedAuditOptions, SimulatedSignalingNetwork};

use super::LostOnTheWay;

/// A request of the peer `S` to the UA `U`, From-tag `from`, To-tag `to`.
fn request(method: &str, cseq: u32, from: &str, to: &str) -> String {
    format!(
        "{method} sip:u@192.0.2.2 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-{cseq}\r\n\
         From: <sip:s@host>;tag={from}\r\nTo: <sip:u@host>;tag={to}\r\nCall-ID: c1@host\r\n\
         CSeq: {cseq} {method}\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
    )
}

/// The fact after the peer sends `sent` in order, its first datagram lost.
async fn lost_after(sent: &[String]) -> LostOnTheWay {
    let peer: SocketAddr = "192.0.2.1:5060".parse().unwrap();
    let first = AtomicBool::new(true);
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
    for raw in sent {
        s.send_to(raw.as_bytes(), u.local_addr()).await.unwrap();
        net.await_in_flight(Duration::from_secs(1)).await;
    }
    LostOnTheWay::from_wire(wrapped.recording.channel())
}

/// The lost request is named by its CSeq and method on its own dialog; the
/// one delivered after it is not lost.
#[tokio::test(start_paused = true)]
async fn the_lost_request_is_named_on_its_dialog() {
    let wire = lost_after(&[request("PRACK", 2, "S", "U"), request("UPDATE", 3, "S", "U")]).await;
    assert_eq!(wire.lost("c1@host", "S", "U"), [(2, "PRACK".to_string())]);
}

/// Another dialog's loss, or the other direction's, is none of this
/// dialog's.
#[tokio::test(start_paused = true)]
async fn a_loss_on_another_dialog_is_not_this_dialogs() {
    let wire = lost_after(&[request("PRACK", 2, "S", "U2"), request("UPDATE", 3, "S", "U")]).await;
    assert!(wire.lost("c1@host", "S", "U").is_empty());
    assert!(wire.lost("c1@host", "U2", "S").is_empty());
    assert!(wire.lost("other@host", "S", "U2").is_empty());
    assert_eq!(wire.lost("c1@host", "S", "U2"), [(2, "PRACK".to_string())]);
}
