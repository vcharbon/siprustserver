//! Capture decoder for SIP triage (test tooling, never in a runner).
//!
//! Reads tcpdump ring files and archived capture corpora — **classic pcap and
//! pcapng, plain or gzipped, detected from the bytes** ([`source`],
//! [`classic`], [`pcapng`]) — walks the link layer (Ethernet / Linux SLL /
//! SLL2 / raw-IP / null-loopback), the IP layer (IPv4 + IPv6, **with fragment
//! reassembly** — a full INVITE with SDP regularly exceeds the MTU), and UDP,
//! yielding `(timestamp, src, dst, payload)` datagrams ready for the real
//! `sip-message` parser.
//!
//! Deliberately hand-rolled instead of pulling a pcap crate: both container
//! formats are tiny, we need zero capture (live) support, and workspace
//! policy keeps dependencies lean.
//!
//! Everything dropped is counted in [`DecodeStats`], never silent — a lossy
//! or odd capture must be visible rather than silently thinning the callflow.
//!
//! Container and frame decoding are this module tree's whole concern. The SIP
//! flow model built on top of it (legs, hops, call-group correlation) lives in
//! [`flow`], its JSON serialization in [`emit`]; the `sipflow` bin is a text
//! presenter over that model.

mod bytes;
mod classic;
pub mod emit;
pub mod flow;
mod frame;
mod pcapng;
mod reassembly;
mod source;

pub use reassembly::{MAX_PENDING_REASSEMBLIES, MAX_REASSEMBLED_LEN, REASSEMBLY_TTL_US};

use std::fmt;
use std::net::SocketAddr;
use std::path::Path;

use reassembly::Reassembler;

/// One decoded UDP datagram from a capture.
#[derive(Debug, Clone)]
pub struct Datagram {
    /// Capture timestamp, microseconds since the Unix epoch.
    pub ts_us: u64,
    pub src: SocketAddr,
    pub dst: SocketAddr,
    pub payload: Vec<u8>,
}

/// Decode counters — surfaced by `sipflow` so a lossy/odd capture is visible
/// instead of silently thinning the callflow.
#[derive(Debug, Default, Clone)]
pub struct DecodeStats {
    /// Packet records seen (all files).
    pub records: u64,
    /// Records whose L2/L3 we could not walk (unknown linktype payload, ARP, …).
    pub non_ip: u64,
    /// IP packets that were not UDP (TCP, ICMP, …).
    pub non_udp: u64,
    /// Records truncated by the snaplen (captured < original) and dropped.
    pub snap_truncated: u64,
    /// UDP datagrams emitted (post-reassembly).
    pub datagrams: u64,
    /// IP fragments consumed by the reassembler.
    pub fragments: u64,
    /// Datagrams successfully reassembled from >1 fragment.
    pub reassembled: u64,
    /// Reassemblies abandoned: expired (missing fragment never captured),
    /// evicted (table full), or over the size cap. These datagrams are LOST —
    /// if this is non-zero near the flow you are chasing, widen the capture
    /// filter or raise the ring size.
    pub frag_dropped: u64,
    /// Trailing partial record at EOF (normal on a live, still-rotating ring).
    pub tail_truncated: u64,
}

impl fmt::Display for DecodeStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "records={} datagrams={} (reassembled={}) skipped: non-ip={} non-udp={} \
             snap-truncated={} frag-dropped={} tail-truncated={}",
            self.records,
            self.datagrams,
            self.reassembled,
            self.non_ip,
            self.non_udp,
            self.snap_truncated,
            self.frag_dropped,
            self.tail_truncated,
        )
    }
}

#[derive(Debug)]
pub enum PcapError {
    Io(std::io::Error),
    /// Not a capture we can walk, or corrupt past the point of resync.
    /// Carries the path and a human-facing reason.
    Format(String),
}

impl fmt::Display for PcapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PcapError::Io(e) => write!(f, "io: {e}"),
            PcapError::Format(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for PcapError {}

impl From<std::io::Error> for PcapError {
    fn from(e: std::io::Error) -> Self {
        PcapError::Io(e)
    }
}

/// Read one or more capture files (in the given order — pass them oldest-first
/// so fragment reassembly can straddle ring-file boundaries) and return every
/// UDP datagram plus decode counters. Each file's container format and
/// compression are detected from its own bytes, so a mixed corpus reads in one
/// call.
pub fn read_capture_files<P: AsRef<Path>>(
    paths: &[P],
) -> Result<(Vec<Datagram>, DecodeStats), PcapError> {
    let mut out = Vec::new();
    let mut stats = DecodeStats::default();
    let mut reasm = Reassembler::new();
    for p in paths {
        let bytes = source::load(p.as_ref())?;
        read_one(&bytes, &mut out, &mut stats, &mut reasm)
            .map_err(|m| PcapError::Format(format!("{}: {m}", p.as_ref().display())))?;
    }
    stats.frag_dropped += reasm.pending_len() as u64; // still-incomplete at EOF
    Ok((out, stats))
}

/// Dispatch on the container magic: pcapng's Section Header Block type is
/// byte-order agnostic (`0a 0d 0d 0a`), classic pcap has four magics.
fn read_one(
    bytes: &[u8],
    out: &mut Vec<Datagram>,
    stats: &mut DecodeStats,
    reasm: &mut Reassembler,
) -> Result<(), String> {
    let magic = match bytes.get(..4) {
        Some(m) => u32::from_le_bytes([m[0], m[1], m[2], m[3]]),
        None => return Err("file shorter than a capture header".into()),
    };
    if magic == pcapng::SHB_TYPE {
        pcapng::walk(bytes, out, stats, reasm)
    } else if classic::MAGICS.contains(&magic) {
        classic::walk(bytes, out, stats, reasm)
    } else {
        Err(format!("not a pcap or pcapng capture (magic {magic:#010x})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a classic-pcap (LE, µs, LINKTYPE_RAW=101) file in memory.
    fn pcap_raw_ip(records: &[(u64, Vec<u8>)]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&0xa1b2_c3d4u32.to_le_bytes());
        f.extend_from_slice(&2u16.to_le_bytes()); // major
        f.extend_from_slice(&4u16.to_le_bytes()); // minor
        f.extend_from_slice(&[0u8; 8]); // thiszone + sigfigs
        f.extend_from_slice(&65535u32.to_le_bytes()); // snaplen
        f.extend_from_slice(&101u32.to_le_bytes()); // linktype RAW
        for (ts_us, pkt) in records {
            f.extend_from_slice(&((ts_us / 1_000_000) as u32).to_le_bytes());
            f.extend_from_slice(&((ts_us % 1_000_000) as u32).to_le_bytes());
            f.extend_from_slice(&(pkt.len() as u32).to_le_bytes());
            f.extend_from_slice(&(pkt.len() as u32).to_le_bytes());
            f.extend_from_slice(pkt);
        }
        f
    }

    /// Build a pcapng (LE) file: SHB, one IDB, then one EPB per record.
    /// `tsresol` is written as `if_tsresol` when `Some`.
    fn pcapng_raw_ip(records: &[(u64, Vec<u8>)], linktype: u16, tsresol: Option<u8>) -> Vec<u8> {
        let mut f = Vec::new();
        // Section Header Block.
        let mut shb = Vec::new();
        shb.extend_from_slice(&0x1a2b_3c4du32.to_le_bytes());
        shb.extend_from_slice(&1u16.to_le_bytes()); // major
        shb.extend_from_slice(&0u16.to_le_bytes()); // minor
        shb.extend_from_slice(&(-1i64).to_le_bytes()); // section length: unknown
        push_block(&mut f, 0x0a0d_0d0a, &shb);
        // Interface Description Block.
        let mut idb = Vec::new();
        idb.extend_from_slice(&linktype.to_le_bytes());
        idb.extend_from_slice(&0u16.to_le_bytes()); // reserved
        idb.extend_from_slice(&65535u32.to_le_bytes()); // snaplen
        if let Some(r) = tsresol {
            idb.extend_from_slice(&9u16.to_le_bytes()); // if_tsresol
            idb.extend_from_slice(&1u16.to_le_bytes());
            idb.extend_from_slice(&[r, 0, 0, 0]); // value + padding
            idb.extend_from_slice(&[0u8; 4]); // opt_endofopt
        }
        push_block(&mut f, 0x0000_0001, &idb);
        for (ticks, pkt) in records {
            let mut epb = Vec::new();
            epb.extend_from_slice(&0u32.to_le_bytes()); // interface id
            epb.extend_from_slice(&((ticks >> 32) as u32).to_le_bytes());
            epb.extend_from_slice(&(*ticks as u32).to_le_bytes());
            epb.extend_from_slice(&(pkt.len() as u32).to_le_bytes()); // captured
            epb.extend_from_slice(&(pkt.len() as u32).to_le_bytes()); // original
            epb.extend_from_slice(pkt);
            epb.resize(epb.len().div_ceil(4) * 4, 0); // packet data padding
            push_block(&mut f, 0x0000_0006, &epb);
        }
        f
    }

    /// Frame a pcapng block: type, total_length, body, total_length.
    fn push_block(out: &mut Vec<u8>, btype: u32, body: &[u8]) {
        let total = (12 + body.len()) as u32;
        out.extend_from_slice(&btype.to_le_bytes());
        out.extend_from_slice(&total.to_le_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(&total.to_le_bytes());
    }

    fn udp_packet(payload: &[u8], sport: u16, dport: u16) -> Vec<u8> {
        let mut u = Vec::new();
        u.extend_from_slice(&sport.to_be_bytes());
        u.extend_from_slice(&dport.to_be_bytes());
        u.extend_from_slice(&((payload.len() + 8) as u16).to_be_bytes());
        u.extend_from_slice(&[0, 0]); // checksum (unverified)
        u.extend_from_slice(payload);
        u
    }

    /// IPv4 header + payload slice, optionally a fragment.
    fn ipv4(payload: &[u8], ident: u16, frag_off_bytes: usize, more: bool) -> Vec<u8> {
        let total = 20 + payload.len();
        let mut p = vec![0u8; 20];
        p[0] = 0x45;
        p[2..4].copy_from_slice(&(total as u16).to_be_bytes());
        p[4..6].copy_from_slice(&ident.to_be_bytes());
        let ff = ((frag_off_bytes / 8) as u16) | if more { 0x2000 } else { 0 };
        p[6..8].copy_from_slice(&ff.to_be_bytes());
        p[8] = 64;
        p[9] = 17; // UDP
        p[12..16].copy_from_slice(&[10, 0, 0, 1]);
        p[16..20].copy_from_slice(&[10, 0, 0, 2]);
        p.extend_from_slice(payload);
        p
    }

    /// Ethernet header with one 802.1Q VLAN tag, carrying IPv4 — the shape
    /// the archived pcapng corpus uses.
    fn eth_vlan(ip: &[u8]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&[0x3c, 0xfd, 0xfe, 0x81, 0x7f, 0x68]); // dst
        f.extend_from_slice(&[0xe0, 0x2f, 0x6d, 0x4a, 0xa8, 0x11]); // src
        f.extend_from_slice(&0x8100u16.to_be_bytes()); // 802.1Q
        f.extend_from_slice(&0x0028u16.to_be_bytes()); // vid
        f.extend_from_slice(&0x0800u16.to_be_bytes()); // IPv4
        f.extend_from_slice(ip);
        f
    }

    fn write_tmp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("sip-pcap-test-{}-{name}", std::process::id()));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn write_tmp_gz(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(bytes).unwrap();
        write_tmp(name, &enc.finish().unwrap())
    }

    #[test]
    fn decodes_a_plain_udp_datagram() {
        let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
        let pkt = ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false);
        let file = write_tmp("plain", &pcap_raw_ip(&[(1_000_000, pkt)]));
        let (dgs, stats) = read_capture_files(&[&file]).unwrap();
        std::fs::remove_file(&file).ok();
        assert_eq!(dgs.len(), 1);
        assert_eq!(dgs[0].payload, msg);
        assert_eq!(dgs[0].src.port(), 5060);
        assert_eq!(dgs[0].dst.port(), 5080);
        assert_eq!(stats.datagrams, 1);
    }

    #[test]
    fn reassembles_two_fragments_even_out_of_order() {
        // One UDP datagram split at an 8-byte-aligned boundary, delivered
        // second-fragment-first.
        let body: Vec<u8> = (0..900u32).map(|i| (i % 251) as u8).collect();
        let udp = udp_packet(&body, 6001, 5060);
        let cut = 600; // multiple of 8
        let f1 = ipv4(&udp[..cut], 7, 0, true);
        let f2 = ipv4(&udp[cut..], 7, cut, false);
        let file = write_tmp("frag", &pcap_raw_ip(&[(1_000_000, f2), (1_000_500, f1)]));
        let (dgs, stats) = read_capture_files(&[&file]).unwrap();
        std::fs::remove_file(&file).ok();
        assert_eq!(dgs.len(), 1, "stats: {stats}");
        assert_eq!(dgs[0].payload, body);
        assert_eq!(stats.reassembled, 1);
        assert_eq!(stats.frag_dropped, 0);
    }

    #[test]
    fn missing_fragment_is_dropped_not_padded() {
        let body: Vec<u8> = vec![7u8; 900];
        let udp = udp_packet(&body, 6001, 5060);
        let f2 = ipv4(&udp[600..], 9, 600, false); // last fragment only
        let file = write_tmp("hole", &pcap_raw_ip(&[(1_000_000, f2)]));
        let (dgs, stats) = read_capture_files(&[&file]).unwrap();
        std::fs::remove_file(&file).ok();
        assert!(dgs.is_empty());
        assert_eq!(stats.frag_dropped, 1); // counted at EOF
    }

    #[test]
    fn stale_pending_reassembly_expires_on_capture_time() {
        let body: Vec<u8> = vec![7u8; 900];
        let udp = udp_packet(&body, 6001, 5060);
        let f1 = ipv4(&udp[..600], 11, 0, true);
        // A later unrelated fragment 60s on (past REASSEMBLY_TTL_US) triggers expiry.
        let other = udp_packet(&[1u8; 16], 1, 2);
        let g1 = ipv4(&other[..8], 12, 0, true);
        let file = write_tmp("stale", &pcap_raw_ip(&[(1_000_000, f1), (61_000_000, g1)]));
        let (dgs, stats) = read_capture_files(&[&file]).unwrap();
        std::fs::remove_file(&file).ok();
        assert!(dgs.is_empty());
        // f1's entry expired (1) + g1 still pending at EOF (1).
        assert_eq!(stats.frag_dropped, 2);
    }

    /// A pcapng capture decodes to the same datagrams as the classic form,
    /// through the Ethernet+VLAN link layer the archived corpus uses.
    #[test]
    fn decodes_a_pcapng_capture() {
        let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
        let frame = eth_vlan(&ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false));
        let file = write_tmp("ng", &pcapng_raw_ip(&[(1_638_412_000_000_000, frame)], 1, None));
        let (dgs, stats) = read_capture_files(&[&file]).unwrap();
        std::fs::remove_file(&file).ok();
        assert_eq!(dgs.len(), 1, "stats: {stats}");
        assert_eq!(dgs[0].payload, msg);
        // No if_tsresol option → pcapng's microsecond default, used verbatim.
        assert_eq!(dgs[0].ts_us, 1_638_412_000_000_000);
    }

    /// `if_tsresol` is honoured in both decimal and binary form: the same
    /// wall-clock instant expressed in ns / µs / ms / 2^-32 s decodes alike.
    #[test]
    fn pcapng_timestamp_resolution_is_honoured() {
        let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
        let frame = eth_vlan(&ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false));
        let secs = 1_638_412_000u64;
        let cases = [
            (9u8, secs * 1_000_000_000), // nanoseconds
            (6u8, secs * 1_000_000),     // microseconds
            (3u8, secs * 1_000),         // milliseconds
            (0x80 | 32, secs << 32),     // binary, 2^-32 s
        ];
        for (resol, ticks) in cases {
            let file = write_tmp(
                &format!("ngres{resol}"),
                &pcapng_raw_ip(&[(ticks, frame.clone())], 1, Some(resol)),
            );
            let (dgs, _) = read_capture_files(&[&file]).unwrap();
            std::fs::remove_file(&file).ok();
            assert_eq!(dgs.len(), 1, "resol {resol:#x}");
            assert_eq!(dgs[0].ts_us, secs * 1_000_000, "resol {resol:#x}");
        }
    }

    /// Compression is detected from the stream, not the file name: the same
    /// bytes gzipped decode identically, for both containers.
    #[test]
    fn gzipped_captures_decode_identically() {
        let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
        let ip = ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false);
        let classic = pcap_raw_ip(&[(1_000_000, ip.clone())]);
        let ng = pcapng_raw_ip(&[(1_000_000, eth_vlan(&ip))], 1, None);
        for (name, bytes) in [("gzclassic", classic), ("gzng", ng)] {
            let plain = write_tmp(name, &bytes);
            let gz = write_tmp_gz(&format!("{name}z"), &bytes);
            let (a, _) = read_capture_files(&[&plain]).unwrap();
            let (b, _) = read_capture_files(&[&gz]).unwrap();
            std::fs::remove_file(&plain).ok();
            std::fs::remove_file(&gz).ok();
            assert_eq!(a.len(), 1, "{name}");
            assert_eq!(a[0].payload, b[0].payload, "{name}");
            assert_eq!(a[0].ts_us, b[0].ts_us, "{name}");
        }
    }

    /// A corpus mixing both containers and both compressions reads in one
    /// call — the format is per-file, never a mode the caller must declare.
    #[test]
    fn mixed_container_corpus_reads_in_one_call() {
        let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
        let ip = ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false);
        let a = write_tmp("mixa", &pcap_raw_ip(&[(1_000_000, ip.clone())]));
        let b = write_tmp_gz("mixb", &pcapng_raw_ip(&[(2_000_000, eth_vlan(&ip))], 1, None));
        let (dgs, stats) = read_capture_files(&[&a, &b]).unwrap();
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
        assert_eq!(dgs.len(), 2, "stats: {stats}");
        assert_eq!(stats.records, 2);
        assert_eq!(dgs[0].ts_us, 1_000_000);
        assert_eq!(dgs[1].ts_us, 2_000_000);
    }

    /// A truncated tail (the capture was still being written) is counted, not
    /// an error: the datagrams before the cut are still returned.
    #[test]
    fn truncated_pcapng_tail_is_counted_not_fatal() {
        let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
        let frame = eth_vlan(&ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false));
        let mut bytes = pcapng_raw_ip(&[(1_000_000, frame.clone()), (2_000_000, frame)], 1, None);
        bytes.truncate(bytes.len() - 40); // cut into the second EPB
        let file = write_tmp("ngtrunc", &bytes);
        let (dgs, stats) = read_capture_files(&[&file]).unwrap();
        std::fs::remove_file(&file).ok();
        assert_eq!(dgs.len(), 1);
        assert_eq!(stats.tail_truncated, 1);
    }

    #[test]
    fn unknown_container_is_a_clear_error() {
        let file = write_tmp("junk", b"not a capture at all, just bytes");
        let err = read_capture_files(&[&file]).unwrap_err();
        std::fs::remove_file(&file).ok();
        assert!(
            format!("{err}").contains("not a pcap or pcapng capture"),
            "unhelpful error: {err}"
        );
    }
}
