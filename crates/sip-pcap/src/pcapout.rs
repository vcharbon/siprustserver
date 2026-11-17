//! Classic-pcap ENCODING: an emitted flows document back into a capture file.
//!
//! The inverse of `crate::classic` for the one shape this crate reads
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
//!
//! The datagrams the reader set aside (the SIP-looking ones the parser
//! rejected, and copies of the accepted ones with a failed header reading)
//! are written the same way, as captured ([`set_aside_to_pcap`]): a capture
//! of what the model could not hold, or could not hold whole.

use std::net::{IpAddr, SocketAddr};

use crate::doc::FlowsDoc;
use crate::flow::FlowStats;
use crate::Datagram;

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
    encode(records)
}

/// The datagrams ingest set aside ([`FlowStats::set_aside_at`]: the ones the
/// parser refused and the accepted ones with a failed header reading), as
/// captured and in capture-time order, as one capture.
pub fn set_aside_to_pcap(stats: &FlowStats, datagrams: &[Datagram]) -> Result<Vec<u8>, String> {
    let mut set_aside: Vec<&Datagram> =
        stats.set_aside_at().filter_map(|i| datagrams.get(i)).collect();
    set_aside.sort_by_key(|d| d.ts_us);
    datagrams_to_pcap(set_aside)
}

/// Encode captured datagrams as they are, their own sockets, timestamps and
/// payload bytes, as a little-endian, microsecond classic pcap: the
/// datagrams a reader set aside, as a capture of their own.
pub fn datagrams_to_pcap<'a>(
    datagrams: impl IntoIterator<Item = &'a Datagram>,
) -> Result<Vec<u8>, String> {
    encode(datagrams.into_iter().map(|d| (d.ts_us, d.src, d.dst, d.payload.clone())).collect())
}

/// One record per `(ts_us, src, dst, payload)`, in timestamp order; stable on
/// ties, so two records stamped alike keep their given order.
fn encode(mut records: Vec<(u64, SocketAddr, SocketAddr, Vec<u8>)>) -> Result<Vec<u8>, String> {
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
/// placeholders — a capture consumer reads nothing from them. The UDP
/// checksum is computed over the pseudo-header of either family (RFC 768,
/// RFC 8200 §8.1), so an IPv6 datagram is one a strict stack accepts.
fn frame(src: SocketAddr, dst: SocketAddr, payload: &[u8]) -> Result<Vec<u8>, String> {
    let udp_len = 8 + payload.len();
    if udp_len > u16::MAX as usize {
        return Err(format!("payload of {} bytes does not fit one UDP datagram", payload.len()));
    }
    let mut f = Vec::with_capacity(14 + 40 + udp_len);
    f.extend_from_slice(&[0x02, 0, 0, 0, 0, 0x02]); // dst MAC
    f.extend_from_slice(&[0x02, 0, 0, 0, 0, 0x01]); // src MAC
    let mut pseudo = Vec::with_capacity(40);
    match (src.ip(), dst.ip()) {
        (IpAddr::V4(s), IpAddr::V4(d)) => {
            f.extend_from_slice(&0x0800u16.to_be_bytes());
            let total = 20 + udp_len;
            let mut h = [0u8; 20];
            h[0] = 0x45;
            h[2..4].copy_from_slice(&(total as u16).to_be_bytes());
            h[6] = 0x40; // DF
            h[8] = 64; // TTL
            h[9] = UDP;
            h[12..16].copy_from_slice(&s.octets());
            h[16..20].copy_from_slice(&d.octets());
            let sum = internet_checksum(&[&h]);
            h[10..12].copy_from_slice(&sum.to_be_bytes());
            f.extend_from_slice(&h);
            pseudo.extend_from_slice(&s.octets());
            pseudo.extend_from_slice(&d.octets());
            pseudo.extend_from_slice(&[0, UDP]);
            pseudo.extend_from_slice(&(udp_len as u16).to_be_bytes());
        }
        (IpAddr::V6(s), IpAddr::V6(d)) => {
            f.extend_from_slice(&0x86ddu16.to_be_bytes());
            let mut h = [0u8; 40];
            h[0] = 0x60;
            h[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
            h[6] = UDP;
            h[7] = 64; // hop limit
            h[8..24].copy_from_slice(&s.octets());
            h[24..40].copy_from_slice(&d.octets());
            f.extend_from_slice(&h);
            pseudo.extend_from_slice(&s.octets());
            pseudo.extend_from_slice(&d.octets());
            pseudo.extend_from_slice(&(udp_len as u32).to_be_bytes());
            pseudo.extend_from_slice(&[0, 0, 0, UDP]);
        }
        _ => return Err(format!("mixed address families {src} → {dst}")),
    }
    let mut udp = [0u8; 8];
    udp[0..2].copy_from_slice(&src.port().to_be_bytes());
    udp[2..4].copy_from_slice(&dst.port().to_be_bytes());
    udp[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
    let sum = match internet_checksum(&[&pseudo, &udp, payload]) {
        0 => 0xffff,
        sum => sum,
    };
    udp[6..8].copy_from_slice(&sum.to_be_bytes());
    f.extend_from_slice(&udp);
    f.extend_from_slice(payload);
    Ok(f)
}

/// The IP protocol number of UDP.
const UDP: u8 = 17;

/// The Internet checksum (RFC 1071) of `parts` read as one byte string: the
/// ones' complement of the ones' complement sum of its big-endian 16-bit
/// words, an odd last byte padded with a zero.
fn internet_checksum(parts: &[&[u8]]) -> u16 {
    let mut sum: u64 = 0;
    for (i, &byte) in parts.iter().flat_map(|p| p.iter()).enumerate() {
        sum += if i % 2 == 0 { u64::from(byte) << 8 } else { u64::from(byte) };
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
        let flows =
            build_flows(&datagrams, &FlowConfig::standard(sip_message::ToolReading::plain_rfc()));
        let doc = flows_to_doc(
            &flows,
            &DecodeStats::default(),
            &EnrichOptions::under(sip_message::ToolReading::plain_rfc()),
        )
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
        assert_eq!(
            build_flows(&back, &FlowConfig::standard(sip_message::ToolReading::plain_rfc()))
                .stats
                .sip_messages,
            2
        );
    }

    /// Each record's UDP segment and the pseudo-header of its family, read
    /// from the written capture's own frames.
    fn udp_segments(pcap: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 24;
        while at < pcap.len() {
            let len = u32::from_le_bytes(pcap[at + 8..at + 12].try_into().unwrap()) as usize;
            let frame = &pcap[at + 16..at + 16 + len];
            at += 16 + len;
            let (segment, pseudo) = match u16::from_be_bytes([frame[12], frame[13]]) {
                0x0800 => {
                    let segment = &frame[34..];
                    let len = (segment.len() as u16).to_be_bytes();
                    (segment, [&frame[26..34], &[0, 17], &len[..]].concat())
                }
                0x86dd => {
                    let segment = &frame[54..];
                    let len = (segment.len() as u32).to_be_bytes();
                    (segment, [&frame[22..54], &len[..], &[0, 0, 0, 17]].concat())
                }
                other => panic!("ethertype {other:#06x}"),
            };
            out.push((pseudo, segment.to_vec()));
        }
        out
    }

    /// The UDP checksum of each written datagram verifies the way a receiving
    /// stack verifies it (RFC 768, RFC 8200 §8.1: the ones' complement sum of
    /// pseudo-header and segment, checksum field included, is all ones), and
    /// is never the zero IPv6 forbids; an odd payload length included.
    #[test]
    fn every_written_udp_checksum_verifies_and_ipv6_is_never_zero() {
        let mut odd = dg(1, "[2001:db8::1]:5060", "[2001:db8::2]:5070");
        odd.payload.push(b'x');
        let datagrams = [
            dg(0, "[2001:db8::1]:5060", "[2001:db8::2]:5070"),
            odd,
            dg(2, "192.0.2.1:5060", "192.0.2.2:5060"),
        ];
        let pcap = datagrams_to_pcap(&datagrams).expect("encodes");
        assert_eq!(read_back(&pcap).1.datagrams, 3, "the reader takes every record");

        let segments = udp_segments(&pcap);
        assert_eq!(segments.len(), 3);
        for (i, (pseudo, segment)) in segments.iter().enumerate() {
            let checksum = u16::from_be_bytes([segment[6], segment[7]]);
            assert_ne!(checksum, 0, "record {i} carries no checksum");
            let mut sum: u32 = 0;
            for word in pseudo.chunks(2).chain(segment.chunks(2)) {
                sum += (u32::from(word[0]) << 8) | u32::from(*word.get(1).unwrap_or(&0));
            }
            let folded = (sum & 0xffff) + (sum >> 16);
            let folded = (folded & 0xffff) + (folded >> 16);
            assert_eq!(folded, 0xffff, "record {i}: checksum {checksum:#06x} does not verify");
        }
    }

    /// The datagrams the parser rejected come back as they were captured.
    #[test]
    fn rejected_datagrams_are_written_as_captured() {
        let garbage = b"INVITE sip:x SIP/2.0\r\nMax-Forwards: 70\r\nContact: <sip:fixture@192.0.2.99>\r\nGarbage\r\n\r\n".to_vec();
        let mut rejected = dg(2_000, "10.0.0.1:5060", "10.0.0.2:5060");
        rejected.payload = garbage.clone();
        let datagrams = vec![dg(1_000, "10.0.0.1:5060", "10.0.0.2:5060"), rejected];
        let flows =
            build_flows(&datagrams, &FlowConfig::standard(sip_message::ToolReading::plain_rfc()));
        let failed = flows.stats.parse_failed_at.iter().map(|&i| &datagrams[i]);
        let (back, stats) = read_back(&datagrams_to_pcap(failed).expect("encodes"));
        assert_eq!(stats.datagrams, 1);
        assert_eq!(
            (back[0].ts_us, back[0].src.to_string(), back[0].payload.clone()),
            (2_000, "10.0.0.1:5060".to_string(), garbage)
        );
    }

    /// An accepted message whose Refer-To the grammar refuses (a `<` never
    /// closed, RFC 3261 §25.1) stays in the model, is counted
    /// `header_failed` with its header and reason, and is copied into the
    /// set-aside capture beside the refused datagram, in capture order.
    #[test]
    fn a_message_with_a_failed_header_reading_is_kept_counted_and_copied() {
        let refer = b"REFER sip:bob@10.0.0.2 SIP/2.0\r\n\
            Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-r1\r\nMax-Forwards: 70\r\n\
            From: <sip:alice@10.0.0.1>;tag=a1\r\nTo: <sip:bob@10.0.0.2>;tag=b1\r\n\
            Call-ID: refer-1@10.0.0.1\r\nCSeq: 2 REFER\r\nContact: <sip:alice@10.0.0.1>\r\n\
            Refer-To: <sip:carol@192.0.2.9:5060\r\n\
            Content-Length: 0\r\n\r\n"
            .to_vec();
        let mut refused = dg(1_000, "10.0.0.1:5060", "10.0.0.2:5060");
        refused.payload = b"INVITE sip:x SIP/2.0\r\nMax-Forwards: 70\r\nContact: <sip:fixture@192.0.2.99>\r\nGarbage\r\n\r\n".to_vec();
        let mut kept = dg(2_000, "10.0.0.1:5060", "10.0.0.2:5060");
        kept.payload = refer.clone();
        let datagrams = vec![kept, refused];
        let flows =
            build_flows(&datagrams, &FlowConfig::standard(sip_message::ToolReading::plain_rfc()));
        assert_eq!((flows.stats.sip_messages, flows.stats.parse_failed), (1, 1));
        assert_eq!((flows.stats.header_failed, flows.stats.header_failed_at.clone()), (1, vec![0]));
        let failure = &flows.stats.header_failures[0];
        assert_eq!((failure.call_id.as_str(), failure.msg), ("refer-1@10.0.0.1", 0));
        assert_eq!(failure.header, "Refer-To");
        assert!(failure.reason.contains("Refer-To"), "{}", failure.reason);
        assert_eq!(flows.legs[0].msgs.len(), 1, "the message stays in the model");

        let doc = flows_to_doc(
            &flows,
            &DecodeStats::default(),
            &EnrichOptions::under(sip_message::ToolReading::plain_rfc()),
        )
        .expect("emits");
        assert_eq!(doc.flow_stats.header_failed, 1);
        assert_eq!(doc.flow_stats.header_failures[0].header, "Refer-To");

        let (back, _) = read_back(&set_aside_to_pcap(&flows.stats, &datagrams).expect("encodes"));
        let ts: Vec<u64> = back.iter().map(|d| d.ts_us).collect();
        assert_eq!(ts, vec![1_000, 2_000], "the refused and the copied one, in capture order");
        assert_eq!(back[1].payload, refer);
    }

    #[test]
    fn a_socket_that_is_not_an_address_is_refused_by_name() {
        let flows = build_flows(
            &[dg(0, "10.0.0.1:5060", "10.0.0.2:5060")],
            &FlowConfig::standard(sip_message::ToolReading::plain_rfc()),
        );
        let mut doc = flows_to_doc(
            &flows,
            &DecodeStats::default(),
            &EnrichOptions::under(sip_message::ToolReading::plain_rfc()),
        )
        .expect("emits");
        doc.legs[0].msgs[0].src = "a".into();
        let err = doc_to_pcap(&doc).expect_err("refused");
        assert!(err.contains("leg 0 msg 0") && err.contains("src"), "{err}");
    }
}
