//! The kernel's IPv4 neighbour table over rtnetlink: one `RTM_GETNEIGH` dump
//! request on a fresh `NETLINK_ROUTE` socket, its answer parsed with checked
//! reads only. A malformed answer is an error, never a panic.

use std::io::Read;
use std::net::Ipv4Addr;
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};

/// One neighbour entry: the interface, the IPv4 address and its NUD state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Neighbour {
    pub ifindex: i32,
    pub addr: Ipv4Addr,
    pub state: u16,
}

/// `NUD_FAILED` (`linux/neighbour.h`): resolution gave up.
pub const NUD_FAILED: u16 = 0x20;

const NLMSG_HDRLEN: usize = 16;
const NDMSG_LEN: usize = 12;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const RTM_NEWNEIGH: u16 = 28;
const RTM_GETNEIGH: u16 = 30;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_DUMP: u16 = 0x300;
const NDA_DST: u16 = 1;
const AF_INET: u8 = 2;
/// The bound on one read of the dump's answer.
const READ_TIMEOUT: Duration = Duration::from_secs(1);

/// The dump of every IPv4 neighbour entry.
pub fn dump_ipv4() -> std::io::Result<Vec<Neighbour>> {
    let mut socket = Socket::new(
        Domain::from(libc::AF_NETLINK),
        Type::RAW,
        Some(Protocol::from(libc::NETLINK_ROUTE)),
    )?;
    socket.set_read_timeout(Some(READ_TIMEOUT))?;
    socket.send(&request(1))?;
    let mut entries = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = socket.read(&mut buf)?;
        let chunk = buf.get(..n).ok_or_else(|| invalid("read past the buffer"))?;
        if parse(chunk, &mut entries)? == Parsed::Done {
            return Ok(entries);
        }
    }
}

/// The `RTM_GETNEIGH` dump request for IPv4, sequence `seq`.
pub fn request(seq: u32) -> Vec<u8> {
    let len = u32::try_from(NLMSG_HDRLEN + NDMSG_LEN).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(NLMSG_HDRLEN + NDMSG_LEN);
    out.extend_from_slice(&len.to_ne_bytes());
    out.extend_from_slice(&RTM_GETNEIGH.to_ne_bytes());
    out.extend_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
    out.extend_from_slice(&seq.to_ne_bytes());
    out.extend_from_slice(&0u32.to_ne_bytes());
    out.push(AF_INET);
    out.extend_from_slice(&[0u8; NDMSG_LEN - 1]);
    out
}

/// Whether the dump's answer continues past a chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parsed {
    More,
    Done,
}

/// Append the IPv4 entries of one chunk of the answer to `out`.
pub fn parse(chunk: &[u8], out: &mut Vec<Neighbour>) -> std::io::Result<Parsed> {
    let mut rest = chunk;
    while !rest.is_empty() {
        let len = usize::try_from(u32_at(rest, 0)?).map_err(|_| invalid("length"))?;
        if len < NLMSG_HDRLEN || len > rest.len() {
            return Err(invalid("message length out of bounds"));
        }
        let msg = rest.get(..len).ok_or_else(|| invalid("message"))?;
        match u16_at(msg, 4)? {
            NLMSG_DONE => return Ok(Parsed::Done),
            NLMSG_ERROR => {
                let code = i32::from_ne_bytes(array(msg, NLMSG_HDRLEN)?);
                if code != 0 {
                    return Err(std::io::Error::from_raw_os_error(code.saturating_neg()));
                }
            }
            RTM_NEWNEIGH => {
                if let Some(n) = neighbour(msg.get(NLMSG_HDRLEN..).unwrap_or(&[]))? {
                    out.push(n);
                }
            }
            _ => {}
        }
        rest = rest.get(align4(len).min(rest.len())..).unwrap_or(&[]);
    }
    Ok(Parsed::More)
}

/// The IPv4 entry of one `RTM_NEWNEIGH` payload; `None` for another family
/// or an entry with no destination.
fn neighbour(payload: &[u8]) -> std::io::Result<Option<Neighbour>> {
    if payload.len() < NDMSG_LEN {
        return Err(invalid("ndmsg truncated"));
    }
    if payload.first() != Some(&AF_INET) {
        return Ok(None);
    }
    let ifindex = i32::from_ne_bytes(array(payload, 4)?);
    let state = u16_at(payload, 8)?;
    let mut attrs = payload.get(NDMSG_LEN..).unwrap_or(&[]);
    while attrs.len() >= 4 {
        let rta_len = usize::from(u16_at(attrs, 0)?);
        if rta_len < 4 || rta_len > attrs.len() {
            return Err(invalid("attribute length out of bounds"));
        }
        if u16_at(attrs, 2)? == NDA_DST {
            let dst: [u8; 4] = array(attrs, 4)?;
            return Ok(Some(Neighbour { ifindex, addr: Ipv4Addr::from(dst), state }));
        }
        attrs = attrs.get(align4(rta_len).min(attrs.len())..).unwrap_or(&[]);
    }
    Ok(None)
}

fn align4(n: usize) -> usize {
    n.saturating_add(3) & !3
}

fn array<const N: usize>(bytes: &[u8], at: usize) -> std::io::Result<[u8; N]> {
    bytes
        .get(at..at.saturating_add(N))
        .and_then(|s| <[u8; N]>::try_from(s).ok())
        .ok_or_else(|| invalid("field truncated"))
}

fn u16_at(bytes: &[u8], at: usize) -> std::io::Result<u16> {
    array(bytes, at).map(u16::from_ne_bytes)
}

fn u32_at(bytes: &[u8], at: usize) -> std::io::Result<u32> {
    array(bytes, at).map(u32::from_ne_bytes)
}

fn invalid(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, format!("neighbour dump: {what}"))
}
