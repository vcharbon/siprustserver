//! The packet capture an endpoint makes of its own datagrams
//! (`CaptureEndpoint`), read back by the capture decoder (`sip-pcap`): each
//! datagram sent and delivered, with its own addresses and bytes, in order;
//! two captures appended to one file read as one capture.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sip_net::{BindUdpOpts, CaptureEndpoint, RealSignalingNetwork, SignalingNetwork, UdpEndpoint};
use tokio::time::timeout;

const OPTIONS: &[u8] =
    b"OPTIONS sip:bob@biloxi.example.com SIP/2.0\r\nCall-ID: capture-1@atlanta.example.com\r\n\r\n";
const OK: &[u8] = b"SIP/2.0 200 OK\r\nCall-ID: capture-1@atlanta.example.com\r\n\r\n";

/// A file path of its own under the temporary directory, `name` in it.
fn scratch(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("sip-net-capture-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

async fn bound(net: &RealSignalingNetwork) -> Arc<dyn UdpEndpoint> {
    net.bind_udp(BindUdpOpts::new("127.0.0.1:0".parse().unwrap(), 64)).await.unwrap().into()
}

/// `captured` sends OPTIONS to `peer`, which answers 200, and receives it.
async fn exchange(captured: &CaptureEndpoint, peer: &dyn UdpEndpoint) {
    captured.send_to(OPTIONS, peer.local_addr()).await.unwrap();
    let asked = timeout(Duration::from_secs(2), peer.recv()).await.unwrap().unwrap();
    peer.send_to(OK, asked.src).await.unwrap();
    let answer = timeout(Duration::from_secs(2), captured.recv()).await.unwrap().unwrap();
    assert_eq!(answer.raw, OK);
}

#[tokio::test]
async fn each_datagram_sent_and_delivered_is_captured_with_its_addresses() {
    let net = RealSignalingNetwork::new();
    let file = scratch("one");
    let peer = bound(&net).await;
    let inner = bound(&net).await;
    let local = inner.local_addr();
    let captured = CaptureEndpoint::open(inner, &file).unwrap();

    exchange(&captured, peer.as_ref()).await;

    let (datagrams, _) = sip_pcap::read_capture_files(&[&file]).unwrap();
    let read: Vec<_> = datagrams.iter().map(|d| (d.src, d.dst, d.payload.as_slice())).collect();
    assert_eq!(read, vec![(local, peer.local_addr(), OPTIONS), (peer.local_addr(), local, OK)]);
    assert!(datagrams[0].ts_us <= datagrams[1].ts_us, "stamped in the order they crossed");
    std::fs::remove_file(&file).unwrap();
}

#[tokio::test]
async fn two_captures_appended_to_one_file_read_as_one() {
    let net = RealSignalingNetwork::new();
    let file = scratch("two");
    let peer = bound(&net).await;
    for _ in 0..2 {
        let captured = CaptureEndpoint::open(bound(&net).await, &file).unwrap();
        exchange(&captured, peer.as_ref()).await;
    }

    let (datagrams, _) = sip_pcap::read_capture_files(&[&file]).unwrap();
    let payloads: Vec<_> = datagrams.iter().map(|d| d.payload.as_slice()).collect();
    assert_eq!(payloads, vec![OPTIONS, OK, OPTIONS, OK]);
    std::fs::remove_file(&file).unwrap();
}
