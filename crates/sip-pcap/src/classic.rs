//! Classic-pcap container walk: a 24-byte global header (magic fixes byte
//! order and timestamp resolution, plus the one linktype every record uses)
//! followed by 16-byte record headers. This file's whole concern is the
//! container; frame decoding lives in [`crate::frame`].

use crate::bytes::u32at;
use crate::frame::decode_frame;
use crate::reassembly::Reassembler;
use crate::{stamp_probe, Datagram, DecodeStats, Probes};

/// Classic-pcap magics, little/big endian × microsecond/nanosecond.
pub const MAGICS: [u32; 4] = [0xa1b2_c3d4, 0xa1b2_3c4d, 0xd4c3_b2a1, 0x4d3c_b2a1];

/// Walk one classic-pcap file. `Err` carries a human-facing reason (the
/// caller prefixes the path).
pub fn walk(
    bytes: &[u8],
    out: &mut Vec<Datagram>,
    stats: &mut DecodeStats,
    reasm: &mut Reassembler,
    probes: &mut Probes,
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
        m => return Err(format!("not a classic pcap file (magic {m:#010x})")),
    };
    let linktype = u32at(bytes, 20, le).ok_or("bad global header")?;
    // A classic-pcap file declares no interfaces: the file IS the observation
    // point.
    let probe = probes.next();

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
        let before = out.len();
        decode_frame(linktype, frame, ts_us, out, stats, reasm);
        stamp_probe(out, before, probe);
    }
}
