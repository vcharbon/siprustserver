//! Classic-pcap ENCODING: an emitted flows document back into a capture file.
//!
//! The inverse of [`crate::classic`] for the one shape this crate reads
//! everything as — Ethernet II → IPv4/IPv6 → UDP → the exact SIP bytes the
//! document carries. An anonymized document is the only form of a production
//! capture that may leave its network; this is what turns it back into a
//! file every capture consumer, this crate's own reader first, takes as a
//! capture.
//!
//! One record per message, in `ts_us` order across every leg, with the
//! message's own `src`/`dst` sockets. Repeats stay repeats and the capture
//! duplicates the reader already collapsed do not come back: the document is
//! the model, and the model is what is written.

use std::net::{IpAddr, SocketAddr};

use crate::doc::FlowsDoc;

/// Ethernet II link type, the one every record is written under.
const LINKTYPE_ETHERNET: u32 = 1;

/// Encode `doc` as a little-endian, microsecond classic pcap.
///
/// A message whose socket text is not an `ip:port`, or whose payload bytes
/// cannot be reassembled, is an error naming it: a capture that silently
/// dropped a message would read as a call that never carried it.
pub fn doc_to_pcap(doc: &FlowsDoc) -> Result<Vec<u8>, String> {
    let mut records: Vec<(u64, SocketAddr, SocketAddr, Vec<u8>)> = Vec::new();
    for (li, leg) in doc.legs.iter().enumerate() {
        for (mi, m) in leg.msgs.iter().enumerate() {
            let at = || format!("leg {li} msg {mi}");
            let src = socket(&m.src).ok_or_else(|| format!("{}: src {:?}", at(), m.src))?;
            let dst = socket(&m.dst).ok_or_else(|| format!("{}: dst {:?}", at(), m.dst))?;
            let payload = m.payload.bytes().map_err(|e| format!("{}: {e}", at()))?;
            records.push((m.ts_us, src, dst, payload));
        }
    }
    // Stable on ties, so two messages the capture stamped alike keep their
    // document order.
    records.sort_by_key(|r| r.0);

    let mut out = Vec::new();
    out.extend_from_slice(&0xa1b2_c3d4u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // version major
    out.extend_from_slice(&4u16.to_le_bytes()); // version minor
    out.extend_from_slice(&0i32.to_le_bytes()); // thiszone
    out.extend_from_slice(&0u32.to_le_bytes()); // sigfigs
    out.extend_from_slice(&262_144u32.to_le_bytes()); // snaplen
    out.extend_from_slice(&LINKTYPE_ETHERNET.to_le_bytes());
    for (ts_us, src, dst, payload) in records {
        let frame = frame(src, dst, &payload)?;
        out.extend_from_slice(&((ts_us / 1_000_000) as u32).to_le_bytes());
        out.extend_from_slice(&((ts_us % 1_000_000) as u32).to_le_bytes());
        out.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        out.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        out.extend_from_slice(&frame);
    }
    Ok(out)
}

/// `ip:port`, the `#label` an endpoint token may carry left off.
fn socket(text: &str) -> Option<SocketAddr> {
    text.split('#').next()?.parse().ok()
}

/// Ethernet II + IP + UDP around `payload`. The MAC addresses are
/// placeholders — a capture consumer reads nothing from them — and the UDP
/// checksum is left at zero, which IPv4 permits and every reader accepts.
fn frame(src: SocketAddr, dst: SocketAddr, payload: &[u8]) -> Result<Vec<u8>, String> {
    let udp_len = 8 + payload.len();
    if udp_len > u16::MAX as usize {
        return Err(format!("payload of {} bytes does not fit one UDP datagram", payload.len()));
    }
    let mut f = Vec::with_capacity(14 + 40 + udp_len);
    f.extend_from_slice(&[0x02, 0, 0, 0, 0, 0x02]); // dst MAC
    f.extend_from_slice(&[0x02, 0, 0, 0, 0, 0x01]); // src MAC
    match (src.ip(), dst.ip()) {
        (IpAddr::V4(s), IpAddr::V4(d)) => {
            f.extend_from_slice(&0x0800u16.to_be_bytes());
            let total = 20 + udp_len;
            let mut h = [0u8; 20];
            h[0] = 0x45;
            h[2..4].copy_from_slice(&(total as u16).to_be_bytes());
            h[6] = 0x40; // DF
            h[8] = 64; // TTL
            h[9] = 17; // UDP
            h[12..16].copy_from_slice(&s.octets());
            h[16..20].copy_from_slice(&d.octets());
            let sum = ipv4_checksum(&h);
            h[10..12].copy_from_slice(&sum.to_be_bytes());
            f.extend_from_slice(&h);
        }
        (IpAddr::V6(s), IpAddr::V6(d)) => {
            f.extend_from_slice(&0x86ddu16.to_be_bytes());
            let mut h = [0u8; 40];
            h[0] = 0x60;
            h[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
            h[6] = 17; // UDP
            h[7] = 64; // hop limit
            h[8..24].copy_from_slice(&s.octets());
            h[24..40].copy_from_slice(&d.octets());
            f.extend_from_slice(&h);
        }
        _ => return Err(format!("mixed address families {src} → {dst}")),
    }
    f.extend_from_slice(&src.port().to_be_bytes());
    f.extend_from_slice(&dst.port().to_be_bytes());
    f.extend_from_slice(&(udp_len as u16).to_be_bytes());
    f.extend_from_slice(&0u16.to_be_bytes()); // checksum: none
    f.extend_from_slice(payload);
    Ok(f)
}

fn ipv4_checksum(header: &[u8; 20]) -> u16 {
    let mut sum: u32 = 0;
    for pair in header.chunks(2) {
        sum += u32::from(u16::from_be_bytes([pair[0], pair[1]]));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emit::flows_to_doc;
    use crate::enrich::EnrichOptions;
    use crate::flow::{build_flows, FlowConfig};
    use crate::{Datagram, DecodeStats};

    const RAW: &str = "OPTIONS sip:b@h SIP/2.0\r\nVia: SIP/2.0/UDP a;branch=z9hG4bK1\r\nFrom: <sip:a@h>;tag=1\r\nTo: <sip:b@h>\r\nCall-ID: c1\r\nCSeq: 1 OPTIONS\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n";

    fn dg(ts_us: u64, src: &str, dst: &str) -> Datagram {
        Datagram {
            ts_us,
            src: src.parse().unwrap(),
            dst: dst.parse().unwrap(),
            payload: RAW.as_bytes().to_vec(),
            probe: 0,
        }
    }

    fn read_back(bytes: &[u8]) -> (Vec<Datagram>, DecodeStats) {
        let dir = std::env::temp_dir().join(format!("pcapout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("t.pcap");
        std::fs::write(&path, bytes).expect("write");
        let out = crate::read_capture_files(&[&path]).expect("reads back");
        std::fs::remove_dir_all(&dir).ok();
        out
    }

    /// What is written is what the reader gets back: sockets, timestamps and
    /// the exact payload bytes, for both address families, in capture order.
    #[test]
    fn the_reader_decodes_what_the_encoder_wrote() {
        let datagrams = vec![
            dg(2_000_001, "10.0.0.1:5060", "10.0.0.2:5060"),
            dg(1_000_000, "[2001:db8::1]:5060", "[2001:db8::2]:5070"),
        ];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        let doc = flows_to_doc(&flows, &DecodeStats::default(), &EnrichOptions::default())
            .expect("emits");
        let (back, stats) = read_back(&doc_to_pcap(&doc).expect("encodes"));

        assert_eq!((stats.records, stats.datagrams), (2, 2));
        let seen: Vec<(u64, String, String)> =
            back.iter().map(|d| (d.ts_us, d.src.to_string(), d.dst.to_string())).collect();
        assert_eq!(
            seen,
            vec![
                (1_000_000, "[2001:db8::1]:5060".to_string(), "[2001:db8::2]:5070".to_string()),
                (2_000_001, "10.0.0.1:5060".to_string(), "10.0.0.2:5060".to_string()),
            ]
        );
        assert!(back.iter().all(|d| d.payload == RAW.as_bytes()));
        assert_eq!(build_flows(&back, &FlowConfig::default()).stats.sip_messages, 2);
    }

    #[test]
    fn a_socket_that_is_not_an_address_is_refused_by_name() {
        let flows = build_flows(&[dg(0, "10.0.0.1:5060", "10.0.0.2:5060")], &FlowConfig::default());
        let mut doc = flows_to_doc(&flows, &DecodeStats::default(), &EnrichOptions::default())
            .expect("emits");
        doc.legs[0].msgs[0].src = "a".into();
        let err = doc_to_pcap(&doc).expect_err("refused");
        assert!(err.contains("leg 0 msg 0") && err.contains("src"), "{err}");
    }
}
