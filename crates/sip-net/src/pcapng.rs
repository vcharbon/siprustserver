//! pcapng encoding of the datagrams one endpoint carries: a section header and
//! its one raw-IP interface open each capture, then one enhanced packet block
//! per datagram, its payload framed as IPv4 (or IPv6) and UDP. Sections
//! appended one after another are one pcapng file, so every capture appended
//! to a file keeps the file readable whole.
//!
//! Little-endian, microsecond timestamps (the format's default resolution).
//! Every UDP checksum is computed over its pseudo header (RFC 768), as IPv6
//! requires (RFC 8200 §8.1).

use std::fs::File;
use std::io::{BufReader, ErrorKind, Read, Seek, SeekFrom};
use std::net::{IpAddr, SocketAddr};

/// The section header block's type.
const SECTION_HEADER: u32 = 0x0A0D_0D0A;
/// The interface description block's type.
const INTERFACE: u32 = 1;
/// The enhanced packet block's type.
const ENHANCED_PACKET: u32 = 6;
/// The byte-order magic, written in the section's own (little-endian) order.
const BYTE_ORDER_MAGIC: u32 = 0x1A2B_3C4D;
/// The interface's link type: raw IP, the version read off the first nibble.
const LINKTYPE_RAW: u16 = 101;
/// The UDP header's length.
const UDP_HEADER: usize = 8;

/// The section header and its one interface: what opens each capture.
pub fn section_header() -> Vec<u8> {
    let mut section = Vec::with_capacity(16);
    section.extend_from_slice(&BYTE_ORDER_MAGIC.to_le_bytes());
    section.extend_from_slice(&1u16.to_le_bytes());
    section.extend_from_slice(&0u16.to_le_bytes());
    section.extend_from_slice(&(-1i64).to_le_bytes());
    let mut interface = Vec::with_capacity(8);
    interface.extend_from_slice(&LINKTYPE_RAW.to_le_bytes());
    interface.extend_from_slice(&0u16.to_le_bytes());
    interface.extend_from_slice(&0u32.to_le_bytes());
    let mut out = block(SECTION_HEADER, &section);
    out.extend_from_slice(&block(INTERFACE, &interface));
    out
}

/// One enhanced packet block on the section's interface: `payload` sent from
/// `src` to `dst`, stamped `ts_us` microseconds after the UNIX epoch. `None`
/// for a payload no UDP datagram holds.
pub fn packet_block(
    ts_us: u64,
    src: SocketAddr,
    dst: SocketAddr,
    payload: &[u8],
) -> Option<Vec<u8>> {
    let packet = ip_packet(src, dst, payload)?;
    let length = u32::try_from(packet.len()).ok()?;
    let mut body = Vec::with_capacity(20 + packet.len() + 3);
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&u32::try_from(ts_us >> 32).unwrap_or(u32::MAX).to_le_bytes());
    body.extend_from_slice(&u32::try_from(ts_us & 0xFFFF_FFFF).unwrap_or(u32::MAX).to_le_bytes());
    body.extend_from_slice(&length.to_le_bytes());
    body.extend_from_slice(&length.to_le_bytes());
    body.extend_from_slice(&packet);
    body.resize(body.len().next_multiple_of(4), 0);
    Some(block(ENHANCED_PACKET, &body))
}

/// A block of `kind` around `body` (its length a multiple of 4): the type,
/// the total length before and after it.
fn block(kind: u32, body: &[u8]) -> Vec<u8> {
    let total = u32::try_from(12 + body.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(12 + body.len());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&total.to_le_bytes());
    out.extend_from_slice(body);
    out.extend_from_slice(&total.to_le_bytes());
    out
}

/// IP and UDP around `payload`: IPv4 where both ends are, else IPv6, an IPv4
/// end written as its mapped IPv6 address. `None` past the UDP length field.
fn ip_packet(src: SocketAddr, dst: SocketAddr, payload: &[u8]) -> Option<Vec<u8>> {
    let udp_length = u16::try_from(UDP_HEADER + payload.len()).ok()?;
    let (mut packet, pseudo) = match (src.ip(), dst.ip()) {
        (IpAddr::V4(from), IpAddr::V4(to)) => {
            let total = udp_length.checked_add(20)?.to_be_bytes();
            let [t0, t1] = total;
            let [a, b, c, d] = from.octets();
            let [e, f, g, h] = to.octets();
            let unsummed = [0x45, 0, t0, t1, 0, 0, 0x40, 0, 64, 17, 0, 0, a, b, c, d, e, f, g, h];
            let [s0, s1] = (!ones_sum(&[&unsummed])).to_be_bytes();
            let [l0, l1] = udp_length.to_be_bytes();
            (
                vec![0x45, 0, t0, t1, 0, 0, 0x40, 0, 64, 17, s0, s1, a, b, c, d, e, f, g, h],
                vec![a, b, c, d, e, f, g, h, 0, 17, l0, l1],
            )
        }
        (from, to) => {
            let mut header = Vec::with_capacity(40);
            header.extend_from_slice(&[0x60, 0, 0, 0]);
            header.extend_from_slice(&udp_length.to_be_bytes());
            header.extend_from_slice(&[17, 64]);
            header.extend_from_slice(&v6(from).octets());
            header.extend_from_slice(&v6(to).octets());
            let mut pseudo = header[8..40].to_vec();
            pseudo.extend_from_slice(&u32::from(udp_length).to_be_bytes());
            pseudo.extend_from_slice(&[0, 0, 0, 17]);
            (header, pseudo)
        }
    };
    let mut udp = Vec::with_capacity(usize::from(udp_length));
    udp.extend_from_slice(&src.port().to_be_bytes());
    udp.extend_from_slice(&dst.port().to_be_bytes());
    udp.extend_from_slice(&udp_length.to_be_bytes());
    udp.extend_from_slice(&0u16.to_be_bytes());
    udp.extend_from_slice(payload);
    let checksum = udp_checksum(&pseudo, &udp);
    udp[6..8].copy_from_slice(&checksum.to_be_bytes());
    packet.extend_from_slice(&udp);
    Some(packet)
}

/// The UDP checksum of the datagram `udp` (its checksum field zero) under the
/// `pseudo` header: the complement of their one's-complement sum, a zero
/// result sent as all ones (RFC 768).
fn udp_checksum(pseudo: &[u8], udp: &[u8]) -> u16 {
    match !ones_sum(&[pseudo, udp]) {
        0 => 0xffff,
        sum => sum,
    }
}

/// `ip` as IPv6: an IPv4 address mapped.
fn v6(ip: IpAddr) -> std::net::Ipv6Addr {
    match ip {
        IpAddr::V4(v4) => v4.to_ipv6_mapped(),
        IpAddr::V6(v6) => v6,
    }
}

/// The one's-complement sum of `parts` read as one run of big-endian 16-bit
/// words (RFC 1071), an odd last byte padded with zero; every part but the
/// last is of even length.
fn ones_sum(parts: &[&[u8]]) -> u16 {
    let mut sum: u64 = parts
        .iter()
        .flat_map(|part| part.chunks(2))
        .map(|pair| match pair {
            [high, low] => u64::from(u16::from_be_bytes([*high, *low])),
            [high] => u64::from(*high) << 8,
            _ => 0,
        })
        .sum();
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    u16::try_from(sum).unwrap_or(u16::MAX)
}

/// The length of the file's leading run of whole blocks: where a capture
/// appended to it starts. A last block whose stated length runs past the end
/// (a write cut short) is not counted. `Err` (`InvalidData`) where the file
/// is no little-endian pcapng this encoder could have written: a first block
/// that is no section header, a section of the other byte order, or a block
/// whose two lengths disagree.
pub fn whole_blocks(file: &File) -> std::io::Result<u64> {
    let end = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(0))?;
    let mut opening = Vec::with_capacity(4);
    (&mut reader).take(4).read_to_end(&mut opening)?;
    if !SECTION_HEADER.to_le_bytes().starts_with(&opening) {
        return Err(foreign("its first block is no section header"));
    }
    reader.seek(SeekFrom::Start(0))?;
    let mut at = 0u64;
    while end - at >= 12 {
        let [kind, total] = [read_u32(&mut reader)?, read_u32(&mut reader)?];
        let total = u64::from(total);
        if total < 12 || total % 4 != 0 {
            return Err(foreign(&format!("the block at byte {at} states a length of {total}")));
        }
        if at + total > end {
            break;
        }
        let mut read = 8;
        if kind == SECTION_HEADER {
            if read_u32(&mut reader)? != BYTE_ORDER_MAGIC {
                return Err(foreign(&format!("the section at byte {at} is not little-endian")));
            }
            read += 4;
        }
        let to_trailer =
            total.checked_sub(4 + read).and_then(|left| i64::try_from(left).ok()).ok_or_else(
                || foreign(&format!("the block at byte {at} is shorter than its fields")),
            )?;
        reader.seek_relative(to_trailer)?;
        if u64::from(read_u32(&mut reader)?) != total {
            return Err(foreign(&format!("the block at byte {at} ends on another length")));
        }
        at += total;
    }
    Ok(at)
}

/// A little-endian word off `reader`.
fn read_u32(reader: &mut impl Read) -> std::io::Result<u32> {
    let mut word = [0u8; 4];
    reader.read_exact(&mut word)?;
    Ok(u32::from_le_bytes(word))
}

/// The refusal of a file holding no capture this encoder appends to.
fn foreign(why: &str) -> std::io::Error {
    std::io::Error::new(ErrorKind::InvalidData, format!("not a pcapng capture to append to: {why}"))
}
