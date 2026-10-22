//! Capture decoder for SIP triage (test tooling, never in a runner).
//!
//! Reads tcpdump ring files and archived capture corpora — **classic pcap and
//! pcapng, plain or gzipped, detected from the bytes** (`source`,
//! `classic`, `pcapng`) — walks the link layer (Ethernet / Linux SLL /
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
pub mod callindex;
mod classic;
pub mod doc;
pub mod emit;
pub mod enrich;
pub mod flow;
mod frame;
pub mod header_errors;
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
mod tests;
