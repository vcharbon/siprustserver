//! Link → IP → UDP decoding of one captured frame into a [`Datagram`].
//!
//! Container-agnostic: classic pcap and pcapng both hand a `(linktype,
//! frame, ts_us)` triple here, so the link layers (Ethernet / Linux SLL /
//! SLL2 / raw-IP / null-loopback), IPv4+IPv6 walking and fragment handoff
//! live in exactly one place.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::bytes::u16be;
use crate::reassembly::{FragKey, FragOutcome, Reassembler};
use crate::{Datagram, DecodeStats};

/// Decode one link-layer frame, appending any complete UDP datagram to `out`.
pub fn decode_frame(
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

fn void_non_udp(stats: &mut DecodeStats) {
    stats.non_udp += 1;
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
                let ident = crate::bytes::u32at(ip, off + 4, false).unwrap_or(0);
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
