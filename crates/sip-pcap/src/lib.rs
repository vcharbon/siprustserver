//! Classic-pcap decoder for SIP triage (test tooling, never in a runner).
//!
//! Reads tcpdump ring files (see `deploy/k8s/sip-capture.sh`), walks the link
//! layer (Ethernet / Linux SLL / SLL2 / raw-IP / null-loopback), the IP layer
//! (IPv4 + IPv6, **with fragment reassembly** — a full INVITE with SDP
//! regularly exceeds the MTU), and UDP, yielding `(timestamp, src, dst,
//! payload)` datagrams ready for the real `sip-message` parser.
//!
//! Deliberately hand-rolled instead of pulling a pcap crate: the format is
//! tiny, we need zero capture (live) support, and workspace policy keeps
//! dependencies lean. pcapng is NOT supported — tcpdump writes classic pcap
//! by default; the reader rejects pcapng with a clear error.
//!
//! Fragment-reassembly protection (lossy captures are the norm — the ring
//! rotates and the BPF filter sees only some ports):
//! - a datagram is emitted ONLY when every byte `0..total_len` is covered;
//!   a hole (missed fragment) is never padded or passed to the SIP stack,
//! - pending reassemblies are bounded ([`MAX_PENDING_REASSEMBLIES`], oldest
//!   evicted first) and size-capped ([`MAX_REASSEMBLED_LEN`]),
//! - pending entries expire after [`REASSEMBLY_TTL_US`] of *capture* time
//!   (pcap timestamps, not wall clock, so offline analysis behaves the same
//!   as live capture would),
//! - everything dropped is counted in [`DecodeStats`], never silent.
//!
//! Datagram decoding is this file's whole concern. The SIP flow model built
//! on top of it (legs, hops, call-group correlation) lives in [`flow`]; the
//! `sipflow` bin is a text presenter over that model.

pub mod flow;

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;

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
    /// pcap records seen (all files).
    pub records: u64,
    /// Records whose L2/L3 we could not walk (unknown linktype payload, ARP, …).
    pub non_ip: u64,
    /// IP packets that were not UDP (TCP, ICMP, …).
    pub non_udp: u64,
    /// Records truncated by the snaplen (incl_len < orig_len) and dropped.
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
    /// Not a classic pcap file (magic mismatch). Carries a hint (e.g. pcapng).
    BadMagic(String),
}

impl fmt::Display for PcapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PcapError::Io(e) => write!(f, "io: {e}"),
            PcapError::BadMagic(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for PcapError {}

impl From<std::io::Error> for PcapError {
    fn from(e: std::io::Error) -> Self {
        PcapError::Io(e)
    }
}

// --- reassembly guards ------------------------------------------------------

/// Max concurrent in-progress reassemblies before the oldest is evicted.
pub const MAX_PENDING_REASSEMBLIES: usize = 4096;
/// Hard cap on a reassembled IP datagram (RFC limit is 64 KiB; SIP is far
/// smaller — anything bigger is garbage or an attack capture).
pub const MAX_REASSEMBLED_LEN: usize = 128 * 1024;
/// A pending reassembly older than this (in capture time) is dropped — its
/// missing fragment was never captured.
pub const REASSEMBLY_TTL_US: u64 = 30_000_000;

/// Reassembly key: RFC 791 (src, dst, protocol, identification); IPv6 uses
/// the fragment-header ident with proto folded in the same way.
#[derive(Hash, PartialEq, Eq, Clone)]
struct FragKey {
    src: IpAddr,
    dst: IpAddr,
    proto: u8,
    ident: u32,
}

struct FragBuf {
    first_seen_us: u64,
    /// (offset, bytes) pieces as captured, in arrival order. Overlaps are
    /// resolved last-writer-wins at assembly time.
    pieces: Vec<(usize, Vec<u8>)>,
    /// Total payload length, known once the MF=0 fragment arrives.
    total_len: Option<usize>,
    bytes_buffered: usize,
}

impl FragBuf {
    /// Assemble iff every byte `0..total` is covered. Holes → `None`.
    fn try_assemble(&self) -> Option<Vec<u8>> {
        let total = self.total_len?;
        if total > MAX_REASSEMBLED_LEN {
            return None;
        }
        let mut buf = vec![0u8; total];
        let mut covered = vec![false; total];
        for (off, bytes) in &self.pieces {
            let end = off.checked_add(bytes.len())?;
            if end > total {
                return None; // fragment past the declared end — corrupt
            }
            buf[*off..end].copy_from_slice(bytes);
            for c in &mut covered[*off..end] {
                *c = true;
            }
        }
        covered.iter().all(|c| *c).then_some(buf)
    }
}

/// IP-fragment reassembler shared across every record of a run (a datagram's
/// fragments may straddle two ring files — feed files in time order).
struct Reassembler {
    pending: HashMap<FragKey, FragBuf>,
    /// Insertion order for oldest-first eviction (coarse; entries may already
    /// be gone when popped — that's fine, we skip them).
    order: Vec<FragKey>,
}

enum FragOutcome {
    /// Complete IP payload (proto, payload).
    Complete(Vec<u8>),
    /// Buffered; more fragments needed.
    Pending,
}

impl Reassembler {
    fn new() -> Self {
        Self { pending: HashMap::new(), order: Vec::new() }
    }

    fn push(
        &mut self,
        stats: &mut DecodeStats,
        ts_us: u64,
        key: FragKey,
        frag_off: usize,
        more_fragments: bool,
        bytes: &[u8],
    ) -> FragOutcome {
        stats.fragments += 1;
        self.expire(stats, ts_us);

        if !self.pending.contains_key(&key) {
            self.order.push(key.clone());
            self.pending.insert(
                key.clone(),
                FragBuf { first_seen_us: ts_us, pieces: Vec::new(), total_len: None, bytes_buffered: 0 },
            );
        }
        let (over_cap, assembled) = {
            let entry = self.pending.get_mut(&key).expect("just inserted");
            entry.bytes_buffered += bytes.len();
            entry.pieces.push((frag_off, bytes.to_vec()));
            if !more_fragments {
                entry.total_len = Some(frag_off + bytes.len());
            }
            // Size guard: a runaway (or hostile) fragment stream is dropped whole.
            let over = entry.bytes_buffered > MAX_REASSEMBLED_LEN;
            (over, if over { None } else { entry.try_assemble() })
        };
        if over_cap {
            self.pending.remove(&key);
            stats.frag_dropped += 1;
            return FragOutcome::Pending;
        }
        if let Some(assembled) = assembled {
            self.pending.remove(&key);
            stats.reassembled += 1;
            return FragOutcome::Complete(assembled);
        }
        // Table-size guard: evict oldest pending entries beyond the cap.
        while self.pending.len() > MAX_PENDING_REASSEMBLIES {
            match self.order.first().cloned() {
                Some(oldest) => {
                    self.order.remove(0);
                    if self.pending.remove(&oldest).is_some() {
                        stats.frag_dropped += 1;
                    }
                }
                None => break,
            }
        }
        FragOutcome::Pending
    }

    /// Drop pending entries whose missing fragments were never captured.
    fn expire(&mut self, stats: &mut DecodeStats, now_us: u64) {
        if self.pending.is_empty() {
            return;
        }
        let before = self.pending.len();
        self.pending.retain(|_, b| now_us.saturating_sub(b.first_seen_us) <= REASSEMBLY_TTL_US);
        stats.frag_dropped += (before - self.pending.len()) as u64;
        if self.pending.is_empty() {
            self.order.clear();
        }
    }
}

// --- byte helpers -----------------------------------------------------------

fn u16be(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(off)?, *b.get(off + 1)?]))
}

fn u32at(b: &[u8], off: usize, le: bool) -> Option<u32> {
    let raw = [*b.get(off)?, *b.get(off + 1)?, *b.get(off + 2)?, *b.get(off + 3)?];
    Some(if le { u32::from_le_bytes(raw) } else { u32::from_be_bytes(raw) })
}

// --- the reader -------------------------------------------------------------

/// Read one or more classic-pcap files (in the given order — pass them
/// oldest-first so fragment reassembly can straddle ring-file boundaries) and
/// return every UDP datagram plus decode counters.
pub fn read_pcap_files<P: AsRef<Path>>(paths: &[P]) -> Result<(Vec<Datagram>, DecodeStats), PcapError> {
    let mut out = Vec::new();
    let mut stats = DecodeStats::default();
    let mut reasm = Reassembler::new();
    for p in paths {
        let bytes = std::fs::read(p)?;
        read_one(&bytes, &mut out, &mut stats, &mut reasm)
            .map_err(|m| PcapError::BadMagic(format!("{}: {m}", p.as_ref().display())))?;
    }
    stats.frag_dropped += reasm.pending.len() as u64; // still-incomplete at EOF
    Ok((out, stats))
}

fn read_one(
    bytes: &[u8],
    out: &mut Vec<Datagram>,
    stats: &mut DecodeStats,
    reasm: &mut Reassembler,
) -> Result<(), String> {
    if bytes.len() < 24 {
        return Err("file shorter than a pcap global header".into());
    }
    let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let (le, ns) = match magic {
        0xa1b2_c3d4 => (true, false),
        0xa1b2_3c4d => (true, true),
        0xd4c3_b2a1 => (false, false),
        0x4d3c_b2a1 => (false, true),
        0x0a0d_0d0a => {
            return Err("pcapng file — not supported; capture with plain tcpdump (classic pcap)".into())
        }
        m => return Err(format!("not a pcap file (magic {m:#010x})")),
    };
    let linktype = u32at(bytes, 20, le).ok_or("bad global header")?;

    let mut off = 24usize;
    loop {
        if off + 16 > bytes.len() {
            if off != bytes.len() {
                stats.tail_truncated += 1;
            }
            return Ok(());
        }
        let ts_sec = u32at(bytes, off, le).unwrap_or(0) as u64;
        let ts_frac = u32at(bytes, off + 4, le).unwrap_or(0) as u64;
        let incl = u32at(bytes, off + 8, le).unwrap_or(0) as usize;
        let orig = u32at(bytes, off + 12, le).unwrap_or(0) as usize;
        off += 16;
        if off + incl > bytes.len() {
            stats.tail_truncated += 1;
            return Ok(());
        }
        let frame = &bytes[off..off + incl];
        off += incl;
        stats.records += 1;
        let ts_us = ts_sec * 1_000_000 + if ns { ts_frac / 1_000 } else { ts_frac };
        if incl < orig {
            // Snapped short: a partial SIP message would parse as garbage (or,
            // worse, as a truncated body) — drop it loudly instead.
            stats.snap_truncated += 1;
            continue;
        }
        decode_frame(linktype, frame, ts_us, out, stats, reasm);
    }
}

fn decode_frame(
    linktype: u32,
    frame: &[u8],
    ts_us: u64,
    out: &mut Vec<Datagram>,
    stats: &mut DecodeStats,
    reasm: &mut Reassembler,
) {
    // Walk L2 → an IP packet (version told by ethertype or first nibble).
    let ip: &[u8] = match linktype {
        // Ethernet, with 802.1Q/802.1ad VLAN tag skipping.
        1 => {
            let mut o = 12usize;
            let mut ethertype = match u16be(frame, o) {
                Some(t) => t,
                None => return void_non_ip(stats),
            };
            while ethertype == 0x8100 || ethertype == 0x88a8 || ethertype == 0x9100 {
                o += 4;
                ethertype = match u16be(frame, o) {
                    Some(t) => t,
                    None => return void_non_ip(stats),
                };
            }
            if ethertype != 0x0800 && ethertype != 0x86dd {
                return void_non_ip(stats);
            }
            &frame[o + 2..]
        }
        // Linux cooked v1 (`-i any` on older libpcap): proto at 14, data at 16.
        113 => {
            match u16be(frame, 14) {
                Some(0x0800) | Some(0x86dd) => {}
                _ => return void_non_ip(stats),
            }
            if frame.len() < 16 {
                return void_non_ip(stats);
            }
            &frame[16..]
        }
        // Linux cooked v2 (`-i any` on current libpcap): proto at 0, data at 20.
        276 => {
            match u16be(frame, 0) {
                Some(0x0800) | Some(0x86dd) => {}
                _ => return void_non_ip(stats),
            }
            if frame.len() < 20 {
                return void_non_ip(stats);
            }
            &frame[20..]
        }
        // Raw IP.
        101 | 12 => frame,
        // BSD null / loopback: 4-byte AF, either byte order.
        0 | 108 => {
            if frame.len() < 4 {
                return void_non_ip(stats);
            }
            &frame[4..]
        }
        _ => return void_non_ip(stats),
    };

    match ip.first().map(|b| b >> 4) {
        Some(4) => decode_ipv4(ip, ts_us, out, stats, reasm),
        Some(6) => decode_ipv6(ip, ts_us, out, stats, reasm),
        _ => void_non_ip(stats),
    }
}

fn void_non_ip(stats: &mut DecodeStats) {
    stats.non_ip += 1;
}

fn decode_ipv4(
    ip: &[u8],
    ts_us: u64,
    out: &mut Vec<Datagram>,
    stats: &mut DecodeStats,
    reasm: &mut Reassembler,
) {
    if ip.len() < 20 {
        return void_non_ip(stats);
    }
    let ihl = ((ip[0] & 0x0f) as usize) * 4;
    let total = u16be(ip, 2).unwrap_or(0) as usize;
    if ihl < 20 || total < ihl || ip.len() < total {
        return void_non_ip(stats);
    }
    let proto = ip[9];
    let src = IpAddr::V4(Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]));
    let dst = IpAddr::V4(Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]));
    let payload = &ip[ihl..total];

    let flags_frag = u16be(ip, 6).unwrap_or(0);
    let more_fragments = flags_frag & 0x2000 != 0;
    let frag_off = ((flags_frag & 0x1fff) as usize) * 8;

    if more_fragments || frag_off != 0 {
        let ident = u16be(ip, 4).unwrap_or(0) as u32;
        let key = FragKey { src, dst, proto, ident };
        match reasm.push(stats, ts_us, key, frag_off, more_fragments, payload) {
            FragOutcome::Complete(full) => emit_udp(proto, &full, src, dst, ts_us, out, stats),
            FragOutcome::Pending => {}
        }
    } else {
        emit_udp(proto, payload, src, dst, ts_us, out, stats);
    }
}

fn decode_ipv6(
    ip: &[u8],
    ts_us: u64,
    out: &mut Vec<Datagram>,
    stats: &mut DecodeStats,
    reasm: &mut Reassembler,
) {
    if ip.len() < 40 {
        return void_non_ip(stats);
    }
    let payload_len = u16be(ip, 4).unwrap_or(0) as usize;
    if ip.len() < 40 + payload_len {
        return void_non_ip(stats);
    }
    let mut src16 = [0u8; 16];
    let mut dst16 = [0u8; 16];
    src16.copy_from_slice(&ip[8..24]);
    dst16.copy_from_slice(&ip[24..40]);
    let src = IpAddr::V6(Ipv6Addr::from(src16));
    let dst = IpAddr::V6(Ipv6Addr::from(dst16));

    let mut nh = ip[6];
    let mut off = 40usize;
    let end = 40 + payload_len;
    // Walk extension headers; a fragment header hands the rest to the reassembler.
    loop {
        match nh {
            // hop-by-hop / routing / destination options
            0 | 43 | 60 => {
                if off + 2 > end {
                    return void_non_ip(stats);
                }
                let next = ip[off];
                let len = (ip[off + 1] as usize + 1) * 8;
                nh = next;
                off += len;
                if off > end {
                    return void_non_ip(stats);
                }
            }
            // fragment header
            44 => {
                if off + 8 > end {
                    return void_non_ip(stats);
                }
                let next = ip[off];
                let fo = u16be(ip, off + 2).unwrap_or(0);
                let frag_off = ((fo >> 3) as usize) * 8;
                let more_fragments = fo & 0x1 != 0;
                let ident = u32at(ip, off + 4, false).unwrap_or(0);
                let frag_payload = &ip[off + 8..end];
                let key = FragKey { src, dst, proto: next, ident };
                match reasm.push(stats, ts_us, key, frag_off, more_fragments, frag_payload) {
                    FragOutcome::Complete(full) => {
                        emit_udp(next, &full, src, dst, ts_us, out, stats)
                    }
                    FragOutcome::Pending => {}
                }
                return;
            }
            17 => {
                emit_udp(17, &ip[off..end], src, dst, ts_us, out, stats);
                return;
            }
            _ => return void_non_udp(stats),
        }
    }
}

fn void_non_udp(stats: &mut DecodeStats) {
    stats.non_udp += 1;
}

fn emit_udp(
    proto: u8,
    payload: &[u8],
    src_ip: IpAddr,
    dst_ip: IpAddr,
    ts_us: u64,
    out: &mut Vec<Datagram>,
    stats: &mut DecodeStats,
) {
    if proto != 17 {
        return void_non_udp(stats);
    }
    if payload.len() < 8 {
        return void_non_udp(stats);
    }
    let sport = u16be(payload, 0).unwrap_or(0);
    let dport = u16be(payload, 2).unwrap_or(0);
    let ulen = u16be(payload, 4).unwrap_or(0) as usize;
    if ulen < 8 || payload.len() < ulen {
        return void_non_udp(stats);
    }
    stats.datagrams += 1;
    out.push(Datagram {
        ts_us,
        src: SocketAddr::new(src_ip, sport),
        dst: SocketAddr::new(dst_ip, dport),
        payload: payload[8..ulen].to_vec(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn write_tmp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("sip-pcap-test-{}-{name}", std::process::id()));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn decodes_a_plain_udp_datagram() {
        let msg = b"OPTIONS sip:x SIP/2.0\r\n\r\n";
        let pkt = ipv4(&udp_packet(msg, 5060, 5080), 1, 0, false);
        let file = write_tmp("plain", &pcap_raw_ip(&[(1_000_000, pkt)]));
        let (dgs, stats) = read_pcap_files(&[&file]).unwrap();
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
        let (dgs, stats) = read_pcap_files(&[&file]).unwrap();
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
        let (dgs, stats) = read_pcap_files(&[&file]).unwrap();
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
        let file =
            write_tmp("stale", &pcap_raw_ip(&[(1_000_000, f1), (61_000_000, g1)]));
        let (dgs, stats) = read_pcap_files(&[&file]).unwrap();
        std::fs::remove_file(&file).ok();
        assert!(dgs.is_empty());
        // f1's entry expired (1) + g1 still pending at EOF (1).
        assert_eq!(stats.frag_dropped, 2);
    }
}
