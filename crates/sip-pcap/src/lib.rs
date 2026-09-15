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
//! Container and frame decoding are this module tree's whole concern. On top
//! of it sit three separable phases: [`flow`] correlates legs into calls
//! (after [`align`] has put a merged capture's probes on one clock),
//! [`query`] selects and projects the calls answering a question (over the
//! transaction view in [`txn`]), and [`emit`] serializes the model. The
//! `sipflow` bin is a presenter over all four.

pub mod align;
mod bytes;
mod callfacts;
mod classic;
pub mod doc;
pub mod emit;
pub mod enrich;
pub mod flow;
mod frame;
mod msgfacts;
mod pcapng;
pub mod pcapout;
pub mod query;
mod reassembly;
pub mod rfc;
mod source;
pub mod txn;

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
    /// WHICH PROBE WROTE THIS COPY, numbered across the whole read: one id per
    /// classic-pcap file, one per pcapng interface per section — or, in a
    /// consecutive set ([`read_capture_set`]), per file from zero. A `mergecap` of
    /// several probes writes one packet once per probe that saw it, and the two
    /// copies are then told apart by this and nothing else — their bytes are
    /// identical and their timestamps differ by the probes' clock offset, which
    /// no window can distinguish from a retransmission.
    pub probe: u32,
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
    /// Files read as one consecutive capture ([`read_capture_set`]) that are
    /// not: the two files and the time between or across them.
    Set(String),
}

impl fmt::Display for PcapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PcapError::Io(e) => write!(f, "io: {e}"),
            PcapError::Format(m) => write!(f, "{m}"),
            PcapError::Set(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for PcapError {}

impl From<std::io::Error> for PcapError {
    fn from(e: std::io::Error) -> Self {
        PcapError::Io(e)
    }
}

/// Hands out the next free probe id. One counter for the whole read (restarted
/// per file of a consecutive set): a container that declares several
/// observation points (pcapng interfaces) takes several, a classic-pcap file
/// takes one.
#[derive(Debug, Default)]
pub struct Probes(u32);

impl Probes {
    pub fn next(&mut self) -> u32 {
        let id = self.0;
        self.0 += 1;
        id
    }

    /// Number the next file's observation points from zero again: the files
    /// of one consecutive capture are slices of one tap, so the k-th
    /// interface of each is one probe, not one per file.
    fn restart(&mut self) {
        self.0 = 0;
    }
}

/// Stamp `probe` on everything appended to `out` since `from`.
///
/// Done by the CONTAINER reader rather than inside the frame decoder, which is
/// container-agnostic by design and must stay ignorant of where a frame was
/// observed.
pub(crate) fn stamp_probe(out: &mut [Datagram], from: usize, probe: u32) {
    for d in &mut out[from..] {
        d.probe = probe;
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
    let (out, stats, _) = read_files(paths, false)?;
    Ok((out, stats))
}

/// How far the first datagram of a ring file may be stamped BEFORE the last
/// datagram of the file before it and still be the same tap: a writer
/// rotating files stamps packets in arrival order on one clock, so anything
/// past a reorder's worth is two captures of one wire, not one capture.
pub const BOUNDARY_SLACK_US: u64 = 100_000;

/// Read several files as ONE capture: consecutive slices of one tap, given
/// oldest first. Read as [`read_capture_files`] reads them, except that every
/// file numbers its probes from zero — the same tap, so the same observation
/// points — then refused as [`PcapError::Set`] unless every file that holds
/// a datagram starts after the previous such file's last datagram — within
/// [`BOUNDARY_SLACK_US`] before it at most — and no more than `max_gap_us`
/// after it. A file with no datagram says nothing about time. The refusal
/// names both files and the gap or overlap, so the caller can say which file
/// is missing, doubled, or out of order.
pub fn read_capture_set<P: AsRef<Path>>(
    paths: &[P],
    max_gap_us: u64,
) -> Result<(Vec<Datagram>, DecodeStats), PcapError> {
    let (out, stats, spans) = read_files(paths, true)?;
    let mut spans = spans.iter().zip(paths).filter_map(|(span, p)| span.map(|s| (s, p.as_ref())));
    if let Some(mut prev) = spans.next() {
        for next in spans {
            let ((first, _), path) = next;
            let ((prev_first, prev_last), prev_path) = prev;
            let apart = |us: u64| format!("{}.{:06} s", us / 1_000_000, us % 1_000_000);
            let refuse = |m: String| {
                PcapError::Set(format!(
                    "not one consecutive capture: {} and {}: {m}",
                    prev_path.display(),
                    path.display()
                ))
            };
            if first < prev_first {
                return Err(refuse(format!(
                    "the second starts {} before the first; pass the files oldest first",
                    apart(prev_first - first)
                )));
            }
            if first + BOUNDARY_SLACK_US < prev_last {
                return Err(refuse(format!("they overlap by {}", apart(prev_last - first))));
            }
            if first > prev_last.saturating_add(max_gap_us) {
                return Err(refuse(format!(
                    "a gap of {} lies between them, more than the {} allowed",
                    apart(first - prev_last),
                    apart(max_gap_us)
                )));
            }
            prev = next;
        }
    }
    Ok((out, stats))
}

/// The first and last datagram timestamp of a file, `None` for a file that
/// yielded no datagram.
type Span = Option<(u64, u64)>;

/// The read behind [`read_capture_files`] and [`read_capture_set`]: every
/// file in the given order through one reassembler, and each file's span.
/// `one_tap` numbers every file's probes from zero (a set); otherwise
/// vantages are numbered across the WHOLE read, so two files' interface 0
/// are two probes and never one.
fn read_files<P: AsRef<Path>>(
    paths: &[P],
    one_tap: bool,
) -> Result<(Vec<Datagram>, DecodeStats, Vec<Span>), PcapError> {
    let mut out = Vec::new();
    let mut stats = DecodeStats::default();
    let mut reasm = Reassembler::new();
    let mut probes = Probes::default();
    let mut spans = Vec::with_capacity(paths.len());
    for p in paths {
        let bytes = source::load(p.as_ref())?;
        let from = out.len();
        if one_tap {
            probes.restart();
        }
        read_one(&bytes, &mut out, &mut stats, &mut reasm, &mut probes)
            .map_err(|m| PcapError::Format(format!("{}: {m}", p.as_ref().display())))?;
        spans.push(out[from..].iter().map(|d| d.ts_us).fold(None, |span: Span, ts| match span {
            None => Some((ts, ts)),
            Some((first, last)) => Some((first.min(ts), last.max(ts))),
        }));
    }
    stats.frag_dropped += reasm.pending_len() as u64; // still-incomplete at EOF
    Ok((out, stats, spans))
}

/// Dispatch on the container magic: pcapng's Section Header Block type is
/// byte-order agnostic (`0a 0d 0d 0a`), classic pcap has four magics.
fn read_one(
    bytes: &[u8],
    out: &mut Vec<Datagram>,
    stats: &mut DecodeStats,
    reasm: &mut Reassembler,
    probes: &mut Probes,
) -> Result<(), String> {
    let magic = match bytes.get(..4) {
        Some(m) => u32::from_le_bytes([m[0], m[1], m[2], m[3]]),
        None => return Err("file shorter than a capture header".into()),
    };
    if magic == pcapng::SHB_TYPE {
        pcapng::walk(bytes, out, stats, reasm, probes)
    } else if classic::MAGICS.contains(&magic) {
        classic::walk(bytes, out, stats, reasm, probes)
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

    /// A pcapng SECTION with `ifaces` interface descriptions, then one EPB per
    /// `(iface_id, ticks, frame)` — a `mergecap` output in miniature.
    fn pcapng_merged(ifaces: usize, records: &[(u32, u64, Vec<u8>)]) -> Vec<u8> {
        let mut f = Vec::new();
        let mut shb = Vec::new();
        shb.extend_from_slice(&0x1a2b_3c4du32.to_le_bytes());
        shb.extend_from_slice(&1u16.to_le_bytes());
        shb.extend_from_slice(&0u16.to_le_bytes());
        shb.extend_from_slice(&(-1i64).to_le_bytes());
        push_block(&mut f, 0x0a0d_0d0a, &shb);
        for _ in 0..ifaces {
            let mut idb = Vec::new();
            idb.extend_from_slice(&1u16.to_le_bytes()); // LINKTYPE_ETHERNET
            idb.extend_from_slice(&0u16.to_le_bytes());
            idb.extend_from_slice(&65535u32.to_le_bytes());
            push_block(&mut f, 0x0000_0001, &idb);
        }
        for (iface, ticks, pkt) in records {
            let mut epb = Vec::new();
            epb.extend_from_slice(&iface.to_le_bytes());
            epb.extend_from_slice(&((ticks >> 32) as u32).to_le_bytes());
            epb.extend_from_slice(&(*ticks as u32).to_le_bytes());
            epb.extend_from_slice(&(pkt.len() as u32).to_le_bytes());
            epb.extend_from_slice(&(pkt.len() as u32).to_le_bytes());
            epb.extend_from_slice(pkt);
            epb.resize(epb.len().div_ceil(4) * 4, 0);
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

    /// One capture split across ring files: each file begins where the last
    /// ended, so the set reads as one stream from one observation point, and
    /// a file with no datagram says nothing about time.
    #[test]
    fn a_consecutive_set_reads_as_one_capture() {
        let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
        let ip = ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false);
        let a =
            write_tmp("seta", &pcap_raw_ip(&[(1_000_000, ip.clone()), (2_000_000, ip.clone())]));
        let empty = write_tmp("setempty", &pcap_raw_ip(&[]));
        let b = write_tmp("setb", &pcap_raw_ip(&[(2_500_000, ip.clone()), (3_000_000, ip)]));
        let read = read_capture_set(&[&a, &empty, &b], 1_000_000);
        for f in [&a, &empty, &b] {
            std::fs::remove_file(f).ok();
        }
        let (dgs, stats) = read.unwrap();
        assert_eq!(stats.records, 4);
        assert_eq!(
            dgs.iter().map(|d| d.ts_us).collect::<Vec<_>>(),
            [1_000_000, 2_000_000, 2_500_000, 3_000_000]
        );
        // One tap: the second file's observation point is the first file's.
        assert!(dgs.iter().all(|d| d.probe == 0), "{dgs:?}");
    }

    /// A fragment split across two ring files is still reassembled: the set is
    /// read with one reassembler, in the given order.
    #[test]
    fn a_consecutive_set_reassembles_across_the_boundary() {
        let payload = vec![b'A'; 1_000];
        let udp = udp_packet(&payload, 5060, 5080);
        let first = ipv4(&udp[..512], 7, 0, true);
        let second = ipv4(&udp[512..], 7, 512, false);
        let a = write_tmp("fraga", &pcap_raw_ip(&[(1_000_000, first)]));
        let b = write_tmp("fragb", &pcap_raw_ip(&[(1_000_100, second)]));
        let read = read_capture_set(&[&a, &b], 1_000_000);
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
        let (dgs, stats) = read.unwrap();
        assert_eq!(stats.reassembled, 1, "stats: {stats}");
        assert_eq!(dgs.len(), 1);
        assert_eq!(dgs[0].payload, payload);
    }

    /// Files that are not consecutive slices of one tap are refused, naming
    /// the two files and what lies between them: a gap wider than the
    /// tolerance, an overlap, or a later file given first. An overlap inside
    /// the boundary slack is a writer's reorder at the rotation, not two
    /// captures of one wire.
    #[test]
    fn a_set_that_is_not_consecutive_is_refused() {
        let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
        let ip = ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false);
        let at = |name: &str, ts: &[u64]| {
            write_tmp(name, &pcap_raw_ip(&ts.iter().map(|&t| (t, ip.clone())).collect::<Vec<_>>()))
        };
        let a = at("nca", &[1_000_000, 2_000_000]);
        let far = at("ncfar", &[5_000_000]);
        let over = at("ncover", &[1_500_000, 2_500_000]);
        let slack = at("ncslack", &[2_000_000 - BOUNDARY_SLACK_US, 2_500_000]);
        let earlier = at("ncearlier", &[500_000]);

        let gap = read_capture_set(&[&a, &far], 1_000_000).unwrap_err().to_string();
        assert!(gap.contains("gap") && gap.contains("3.000000 s"), "{gap}");
        assert!(gap.contains("nca") && gap.contains("ncfar"), "{gap}");
        // The same gap inside a wider tolerance is consecutive.
        assert!(read_capture_set(&[&a, &far], 3_000_000).is_ok());

        let overlap = read_capture_set(&[&a, &over], 1_000_000).unwrap_err().to_string();
        assert!(overlap.contains("overlap") && overlap.contains("0.500000 s"), "{overlap}");

        assert!(read_capture_set(&[&a, &slack], 1_000_000).is_ok());

        let order = read_capture_set(&[&a, &earlier], 1_000_000).unwrap_err().to_string();
        assert!(order.contains("oldest first"), "{order}");

        for f in [&a, &far, &over, &slack, &earlier] {
            std::fs::remove_file(f).ok();
        }
    }

    /// A `mergecap` of two probes: the SAME packet, written once per probe, is
    /// two datagrams identical in every field but the probe that wrote them.
    /// Nothing else in the model can tell the two copies apart.
    #[test]
    fn each_pcapng_interface_is_its_own_probe() {
        let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
        let frame = eth_vlan(&ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false));
        // Probe 1's clock runs 8 s ahead of probe 0's — far outside any
        // capture-dedup window, and squarely on the INVITE retransmit ladder.
        let file = write_tmp(
            "twoprobes",
            &pcapng_merged(2, &[(0, 1_000_000, frame.clone()), (1, 9_000_000, frame)]),
        );
        let (dgs, _) = read_capture_files(&[&file]).unwrap();
        std::fs::remove_file(&file).ok();
        assert_eq!(dgs.len(), 2);
        assert_eq!(dgs[0].payload, dgs[1].payload, "one packet, two records");
        assert_eq!((dgs[0].probe, dgs[1].probe), (0, 1), "one probe per interface");
    }

    /// Probes are numbered across the WHOLE read, so two files' interface 0 are
    /// two observation points and never one.
    #[test]
    fn probes_are_numbered_across_every_file_read() {
        let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
        let ip = ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false);
        let a = write_tmp("pa", &pcap_raw_ip(&[(1_000_000, ip.clone())]));
        let b = write_tmp("pb", &pcapng_merged(2, &[(0, 2_000_000, eth_vlan(&ip))]));
        let (dgs, _) = read_capture_files(&[&a, &b]).unwrap();
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
        assert_eq!(dgs.iter().map(|d| d.probe).collect::<Vec<_>>(), vec![0, 1]);
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
