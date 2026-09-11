//! pcapng container walk: a block stream (`type`, `total_length`, body,
//! `total_length` again) whose byte order and per-interface link type and
//! timestamp resolution are declared in-band. This file's whole concern is
//! the container; frame decoding lives in [`crate::frame`].
//!
//! Only packet-bearing blocks are consumed — Enhanced Packet (6), Simple
//! Packet (3) and the obsolete Packet (2). Every other block type (name
//! resolution, interface statistics, decryption secrets, custom) carries no
//! captured frame, so skipping it drops nothing. A Section Header resets the
//! interface table: interface ids are section-scoped, and a `mergecap` output
//! may concatenate sections.

use crate::bytes::{u16at, u32at};
use crate::frame::decode_frame;
use crate::reassembly::Reassembler;
use crate::{stamp_probe, Datagram, DecodeStats, Probes};

/// Section Header Block type — also the file magic of a pcapng capture.
pub const SHB_TYPE: u32 = 0x0a0d_0d0a;
const IDB_TYPE: u32 = 0x0000_0001;
const PB_TYPE: u32 = 0x0000_0002;
const SPB_TYPE: u32 = 0x0000_0003;
const EPB_TYPE: u32 = 0x0000_0006;

/// Byte-order magic inside a Section Header Block body.
const BYTE_ORDER_MAGIC: u32 = 0x1a2b_3c4d;

/// `if_tsresol` — timestamp resolution, one byte (see [`Iface::ts_us`]).
const OPT_IF_TSRESOL: u16 = 9;
/// `if_tsoffset` — seconds added to every timestamp on this interface.
const OPT_IF_TSOFFSET: u16 = 14;

/// A block shorter than its own header+trailer, or not 4-byte aligned, is
/// corrupt: the walk stops rather than resyncing on garbage.
const MIN_BLOCK_LEN: usize = 12;

/// One interface of the current section.
struct Iface {
    linktype: u32,
    /// `if_tsresol` as written: high bit set → 2^-n, else 10^-n.
    tsresol: u8,
    /// `if_tsoffset`, seconds.
    tsoffset_s: u64,
    /// The observation point this interface IS. A `mergecap` output keeps one
    /// interface per input file, so this is what tells one probe's copy of a
    /// packet from another's.
    probe: u32,
}

impl Iface {
    /// pcapng default when the interface declares no `if_tsresol`: microseconds.
    fn new(linktype: u32) -> Self {
        Self { linktype, tsresol: 6, tsoffset_s: 0, probe: 0 }
    }

    /// Raw tick count → microseconds since the epoch.
    fn ts_us(&self, ticks: u64) -> u64 {
        let ticks = ticks as u128;
        let frac_us = if self.tsresol & 0x80 != 0 {
            // Binary: one tick is 2^-n seconds.
            let n = (self.tsresol & 0x7f) as u32;
            ticks.saturating_mul(1_000_000) >> n.min(127)
        } else {
            // Decimal: one tick is 10^-n seconds.
            let n = self.tsresol as u32;
            match n.cmp(&6) {
                std::cmp::Ordering::Less => ticks.saturating_mul(10u128.pow(6 - n)),
                std::cmp::Ordering::Equal => ticks,
                std::cmp::Ordering::Greater => ticks / 10u128.pow((n - 6).min(38)),
            }
        };
        let total = frac_us.saturating_add((self.tsoffset_s as u128).saturating_mul(1_000_000));
        u64::try_from(total).unwrap_or(u64::MAX)
    }
}

/// Walk one pcapng file. `Err` carries a human-facing reason (the caller
/// prefixes the path); a truncated tail is counted, not an error — a still-
/// rotating capture is normal input.
pub fn walk(
    bytes: &[u8],
    out: &mut Vec<Datagram>,
    stats: &mut DecodeStats,
    reasm: &mut Reassembler,
    probes: &mut Probes,
) -> Result<(), String> {
    let mut off = 0usize;
    let mut le = true;
    let mut ifaces: Vec<Iface> = Vec::new();
    let mut sections = 0u32;

    while off + 8 <= bytes.len() {
        let btype = u32at(bytes, off, le).expect("bounds checked");
        if btype == SHB_TYPE {
            // The section's own byte order is declared inside its body, so the
            // BOM is read before this block's own total_length.
            le = match u32at(bytes, off + 8, true) {
                Some(BYTE_ORDER_MAGIC) => true,
                _ => match u32at(bytes, off + 8, false) {
                    Some(BYTE_ORDER_MAGIC) => false,
                    _ if sections == 0 => {
                        return Err("pcapng section header has no byte-order magic".into())
                    }
                    _ => {
                        stats.tail_truncated += 1;
                        return Ok(());
                    }
                },
            };
            ifaces.clear();
            sections += 1;
        }
        let total = match u32at(bytes, off + 4, le) {
            Some(t) => t as usize,
            None => break,
        };
        if total < MIN_BLOCK_LEN || total % 4 != 0 {
            return Err(format!("corrupt pcapng block at offset {off}: total_length={total}"));
        }
        if off + total > bytes.len() {
            stats.tail_truncated += 1;
            return Ok(());
        }
        let body = &bytes[off + 8..off + total - 4];
        match btype {
            IDB_TYPE => {
                // Interface ids are section-scoped, so each one takes its own
                // probe id: two sections' interface 0 are two probes.
                let mut iface = parse_idb(body, le);
                iface.probe = probes.next();
                ifaces.push(iface);
            }
            EPB_TYPE | PB_TYPE | SPB_TYPE => {
                packet_block(btype, body, le, &ifaces, out, stats, reasm)
            }
            _ => {}
        }
        off += total;
    }
    if off != bytes.len() {
        stats.tail_truncated += 1;
    }
    if sections == 0 {
        return Err("not a pcapng file (no section header block)".into());
    }
    Ok(())
}

fn parse_idb(body: &[u8], le: bool) -> Iface {
    let mut iface = Iface::new(u16at(body, 0, le).unwrap_or(0) as u32);
    // Options follow linktype(2) + reserved(2) + snaplen(4).
    for (code, value) in options(body.get(8..).unwrap_or_default(), le) {
        match (code, value) {
            (OPT_IF_TSRESOL, [resol, ..]) => iface.tsresol = *resol,
            (OPT_IF_TSOFFSET, _) if value.len() >= 8 => {
                let lo = u32at(value, 0, le).unwrap_or(0) as u64;
                let hi = u32at(value, 4, le).unwrap_or(0) as u64;
                iface.tsoffset_s = if le { (hi << 32) | lo } else { (lo << 32) | hi };
            }
            _ => {}
        }
    }
    iface
}

/// Block options: `(code u16, len u16, value padded to 4)` until `opt_endofopt`.
fn options(mut body: &[u8], le: bool) -> Vec<(u16, &[u8])> {
    let mut out = Vec::new();
    while body.len() >= 4 {
        let code = u16at(body, 0, le).expect("bounds checked");
        let len = u16at(body, 2, le).expect("bounds checked") as usize;
        if code == 0 {
            break;
        }
        let padded = len.div_ceil(4) * 4;
        if 4 + len > body.len() {
            break;
        }
        out.push((code, &body[4..4 + len]));
        if 4 + padded > body.len() {
            break;
        }
        body = &body[4 + padded..];
    }
    out
}

/// Extract `(interface, ts_ticks, captured, original)` from a packet-bearing
/// block body and hand the frame to the decoder.
fn packet_block(
    btype: u32,
    body: &[u8],
    le: bool,
    ifaces: &[Iface],
    out: &mut Vec<Datagram>,
    stats: &mut DecodeStats,
    reasm: &mut Reassembler,
) {
    let (iface_id, ticks, cap_len, orig_len, data_off) = match btype {
        EPB_TYPE => {
            let Some(id) = u32at(body, 0, le) else { return };
            let hi = u32at(body, 4, le).unwrap_or(0) as u64;
            let lo = u32at(body, 8, le).unwrap_or(0) as u64;
            let cap = u32at(body, 12, le).unwrap_or(0) as usize;
            let orig = u32at(body, 16, le).unwrap_or(0) as usize;
            (id as usize, (hi << 32) | lo, cap, orig, 20usize)
        }
        PB_TYPE => {
            let Some(id) = u16at(body, 0, le) else { return };
            let hi = u32at(body, 4, le).unwrap_or(0) as u64;
            let lo = u32at(body, 8, le).unwrap_or(0) as u64;
            let cap = u32at(body, 12, le).unwrap_or(0) as usize;
            let orig = u32at(body, 16, le).unwrap_or(0) as usize;
            (id as usize, (hi << 32) | lo, cap, orig, 20usize)
        }
        // Simple Packet: no interface id and no timestamp — it always rides
        // interface 0, and the capture length is whatever the block holds.
        _ => {
            let Some(orig) = u32at(body, 0, le) else { return };
            (0usize, 0u64, body.len() - 4, orig as usize, 4usize)
        }
    };
    stats.records += 1;
    let Some(iface) = ifaces.get(iface_id) else {
        // A packet naming an interface the section never described: the frame
        // cannot be decoded (unknown link type), so it is dropped, not guessed.
        stats.non_ip += 1;
        return;
    };
    if cap_len < orig_len {
        // Snapped short: a partial SIP message would parse as garbage.
        stats.snap_truncated += 1;
        return;
    }
    let Some(frame) = body.get(data_off..data_off + cap_len) else {
        stats.tail_truncated += 1;
        return;
    };
    let before = out.len();
    decode_frame(iface.linktype, frame, iface.ts_us(ticks), out, stats, reasm);
    stamp_probe(out, before, iface.probe);
}
