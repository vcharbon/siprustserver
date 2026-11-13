//! The packet capture an endpoint makes of its own datagrams
//! (`CaptureEndpoint`), read back by the capture decoder (`sip-pcap`): each
//! datagram sent and delivered, with its own addresses, bytes and wall-clock
//! time, in order, over IPv4 and IPv6, its UDP checksum one a receiver
//! accepts at an even and an odd length; two captures appended to one file
//! read as one capture, a block an earlier capture left cut short dropped, a
//! tail no capture's write leaves refused. An endpoint bound to the
//! unspecified address, whose own address no datagram states, is refused.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sip_net::{BindUdpOpts, CaptureEndpoint, RealSignalingNetwork, SignalingNetwork, UdpEndpoint};
use tokio::time::timeout;

const OPTIONS: &[u8] =
    b"OPTIONS sip:bob@biloxi.example.com SIP/2.0\r\nCall-ID: capture-1@atlanta.example.com\r\n\r\n";
/// Of odd length, where [`OPTIONS`] is of even length: the checksum pads its
/// last byte.
const OK: &[u8] =
    b"SIP/2.0 200 OK\r\nCall-ID: capture-1@atlanta.example.com\r\nContent-Length: 0\r\n\r\n";

/// A file path of its own under the temporary directory, `name` in it.
fn scratch(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("sip-net-capture-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// An endpoint bound to a port of the OS's choosing at `ip`.
async fn bound_at(net: &RealSignalingNetwork, ip: &str) -> Arc<dyn UdpEndpoint> {
    let at = SocketAddr::new(ip.parse().unwrap(), 0);
    net.bind_udp(BindUdpOpts::new(at, 64)).await.unwrap().into()
}

async fn bound(net: &RealSignalingNetwork) -> Arc<dyn UdpEndpoint> {
    bound_at(net, "127.0.0.1").await
}

/// `captured` sends OPTIONS to `peer`, which answers 200, and receives it:
/// one datagram of even length and one of odd length.
async fn exchange(captured: &CaptureEndpoint, peer: &dyn UdpEndpoint) {
    assert_eq!((OPTIONS.len() % 2, OK.len() % 2), (0, 1));
    captured.send_to(OPTIONS, peer.local_addr()).await.unwrap();
    let asked = timeout(Duration::from_secs(2), peer.recv()).await.unwrap().unwrap();
    peer.send_to(OK, asked.src).await.unwrap();
    let answer = timeout(Duration::from_secs(2), captured.recv()).await.unwrap().unwrap();
    assert_eq!(answer.raw, OK);
}

/// Microseconds since the UNIX epoch on the wall clock.
fn wall_us() -> u64 {
    u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros()).unwrap()
}

/// Each enhanced packet block's packet in a little-endian pcapng file, as the
/// file's bytes hold it.
fn packets(file: &Path) -> Vec<Vec<u8>> {
    let bytes = std::fs::read(file).unwrap();
    let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let (kind, total) = (word(at), word(at + 4));
        if kind == 6 {
            let captured = word(at + 20);
            out.push(bytes[at + 28..at + 28 + captured].to_vec());
        }
        at += total;
    }
    out
}

/// Whether the UDP checksum of the raw IP `packet` verifies as a receiver
/// checks it (RFC 768, RFC 8200 §8.1): the one's-complement sum of the pseudo
/// header and the whole datagram, checksum included, is all ones, and the
/// checksum is not the zero IPv6 forbids.
fn udp_checksum_verifies(packet: &[u8]) -> bool {
    let (mut summed, udp) = match packet[0] >> 4 {
        4 => {
            let udp = &packet[20..];
            let mut pseudo = packet[12..20].to_vec();
            pseudo.extend_from_slice(&[0, 17]);
            pseudo.extend_from_slice(&udp[4..6]);
            (pseudo, udp)
        }
        _ => {
            let udp = &packet[40..];
            let mut pseudo = packet[8..40].to_vec();
            pseudo.extend_from_slice(&u32::try_from(udp.len()).unwrap().to_be_bytes());
            pseudo.extend_from_slice(&[0, 0, 0, 17]);
            (pseudo, udp)
        }
    };
    summed.extend_from_slice(udp);
    if summed.len() % 2 == 1 {
        summed.push(0);
    }
    let mut sum: u32 =
        summed.chunks(2).map(|pair| u32::from(u16::from_be_bytes([pair[0], pair[1]]))).sum();
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum == 0xffff && udp[6..8] != [0, 0]
}

#[tokio::test]
async fn each_datagram_sent_and_delivered_is_captured_with_its_addresses_and_time() {
    let net = RealSignalingNetwork::new();
    let file = scratch("one");
    let peer = bound(&net).await;
    let inner = bound(&net).await;
    let local = inner.local_addr();
    let captured = CaptureEndpoint::open(inner, &file).unwrap();

    let before = wall_us();
    exchange(&captured, peer.as_ref()).await;
    let after = wall_us();

    let (datagrams, _) = sip_pcap::read_capture_files(&[&file]).unwrap();
    let read: Vec<_> = datagrams.iter().map(|d| (d.src, d.dst, d.payload.as_slice())).collect();
    assert_eq!(read, vec![(local, peer.local_addr(), OPTIONS), (peer.local_addr(), local, OK)]);
    for datagram in &datagrams {
        assert!(
            (before..=after).contains(&datagram.ts_us),
            "{} not in {before}..={after}",
            datagram.ts_us
        );
    }
    assert!(datagrams[0].ts_us <= datagrams[1].ts_us, "stamped in the order they crossed");
    let packets = packets(&file);
    assert!(packets.iter().all(|p| udp_checksum_verifies(p)), "every UDP checksum verifies");
    std::fs::remove_file(&file).unwrap();
}

#[tokio::test]
async fn an_ipv6_endpoints_datagrams_are_captured_as_ipv6_with_their_checksums() {
    let net = RealSignalingNetwork::new();
    let file = scratch("six");
    let peer = bound_at(&net, "::1").await;
    let inner = bound_at(&net, "::1").await;
    let local = inner.local_addr();
    let captured = CaptureEndpoint::open(inner, &file).unwrap();

    exchange(&captured, peer.as_ref()).await;

    let (datagrams, _) = sip_pcap::read_capture_files(&[&file]).unwrap();
    let read: Vec<_> = datagrams.iter().map(|d| (d.src, d.dst, d.payload.as_slice())).collect();
    assert_eq!(read, vec![(local, peer.local_addr(), OPTIONS), (peer.local_addr(), local, OK)]);
    let packets = packets(&file);
    assert_eq!(packets.iter().map(|p| p[0] >> 4).collect::<Vec<_>>(), [6, 6]);
    assert!(packets.iter().all(|p| udp_checksum_verifies(p)), "every UDP checksum verifies");
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

/// A capture opened on a file an earlier capture left ending in a block cut
/// short (its write refused midway) drops that block, so the file reads whole.
#[tokio::test]
async fn a_capture_opened_after_a_block_cut_short_drops_it_and_the_file_reads_whole() {
    let net = RealSignalingNetwork::new();
    let file = scratch("cut");
    let peer = bound(&net).await;
    let first = CaptureEndpoint::open(bound(&net).await, &file).unwrap();
    exchange(&first, peer.as_ref()).await;
    drop(first);
    let mut bytes = std::fs::read(&file).unwrap();
    bytes.extend_from_slice(&[6, 0, 0, 0, 0x60, 0, 0, 0, 0, 0]);
    std::fs::write(&file, &bytes).unwrap();

    let second = CaptureEndpoint::open(bound(&net).await, &file).unwrap();
    exchange(&second, peer.as_ref()).await;

    let (datagrams, _) = sip_pcap::read_capture_files(&[&file]).unwrap();
    let payloads: Vec<_> = datagrams.iter().map(|d| d.payload.as_slice()).collect();
    assert_eq!(payloads, vec![OPTIONS, OK, OPTIONS, OK]);
    std::fs::remove_file(&file).unwrap();
}

/// A file whose last block states a length past any block a capture writes
/// is refused and kept as it was: its tail is no write of a capture cut short.
#[tokio::test]
async fn a_tail_stating_a_block_longer_than_any_capture_writes_is_refused_and_kept() {
    let net = RealSignalingNetwork::new();
    let file = scratch("long-tail");
    let first = CaptureEndpoint::open(bound(&net).await, &file).unwrap();
    drop(first);
    let mut bytes = std::fs::read(&file).unwrap();
    bytes.extend_from_slice(&6u32.to_le_bytes());
    bytes.extend_from_slice(&(1u32 << 20).to_le_bytes());
    bytes.extend_from_slice(&[0; 64]);
    std::fs::write(&file, &bytes).unwrap();

    let refused = CaptureEndpoint::open(bound(&net).await, &file).err().expect("refused");

    assert_eq!(refused.kind(), std::io::ErrorKind::InvalidData, "{refused}");
    assert_eq!(std::fs::read(&file).unwrap(), bytes);
    std::fs::remove_file(&file).unwrap();
}

/// A file that holds no pcapng capture is refused and kept as it was: a
/// capture appends to its own file only.
#[tokio::test]
async fn a_file_holding_no_capture_is_refused_and_kept() {
    let net = RealSignalingNetwork::new();
    let file = scratch("foreign");
    std::fs::write(&file, b"not a capture\n").unwrap();

    let refused = CaptureEndpoint::open(bound(&net).await, &file).err().expect("refused");

    assert_eq!(refused.kind(), std::io::ErrorKind::InvalidData, "{refused}");
    assert_eq!(std::fs::read(&file).unwrap(), b"not a capture\n");
    std::fs::remove_file(&file).unwrap();
}

/// An endpoint bound to the unspecified address is refused before its file
/// is made: a datagram's own address would read as `0.0.0.0`.
#[tokio::test]
async fn an_endpoint_bound_to_the_unspecified_address_is_refused() {
    let net = RealSignalingNetwork::new();
    let file = scratch("unspecified");
    let inner = bound_at(&net, "0.0.0.0").await;
    assert_eq!(inner.local_addr().ip(), IpAddr::from([0, 0, 0, 0]));

    let refused = CaptureEndpoint::open(inner, &file).err().expect("refused");

    assert_eq!(refused.kind(), std::io::ErrorKind::InvalidInput, "{refused}");
    assert!(refused.to_string().contains("0.0.0.0"), "{refused}");
    assert!(!file.exists());
}
