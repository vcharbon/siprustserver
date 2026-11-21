//! Loopback addresses a test hands a process it spawns. Each is free on this
//! box when it is picked, none repeats another of the same call, and each
//! lies below the kernel's ephemeral range: no other process's bind on port 0
//! nor outbound connect takes one before the spawned process binds it, as a
//! port read off a bind on port 0 and released can be.

use std::hash::{BuildHasher, RandomState};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};

/// The socket kind an address is picked free for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Udp,
    Tcp,
}

/// The lowest port picked, where the ephemeral range leaves [`SPAN`] ports above it.
const FLOOR: u16 = 10_000;

/// The lowest unprivileged port, the floor where the ephemeral range starts too low for [`FLOOR`].
const UNPRIVILEGED: u16 = 1024;

/// The fewest ports a pick needs below the ephemeral range.
const SPAN: u16 = 1000;

/// The ephemeral range's low bound where the kernel does not state it, the IANA dynamic range's being higher.
const EPHEMERAL_LOW: u16 = 32_768;

/// How many candidates one address skips, taken or repeated, before the pick fails.
const TRIES: usize = 100;

/// Where Linux states its ephemeral range, `<low> <high>`.
const RANGE_FILE: &str = "/proc/sys/net/ipv4/ip_local_port_range";

/// A loopback address for each of `kinds`, in order, picked at random below
/// the ephemeral range, each checked free by binding it and all held until
/// all are picked. `Err` where the range leaves fewer than 1000
/// unprivileged ports below it, or where an address finds no free port in
/// 100 candidates.
pub fn free_addresses(kinds: &[Kind]) -> io::Result<Vec<SocketAddr>> {
    let (floor, ceiling) = range_below(ephemeral_low()?)?;
    let random = RandomState::new();
    let mut drawn = 0u64;
    pick(kinds, || {
        drawn += 1;
        floor + offset_below(random.hash_one(drawn), ceiling - floor)
    })
}

/// `hash` reduced below `span`, so it fits the `u16` it is cast to.
fn offset_below(hash: u64, span: u16) -> u16 {
    (hash % u64::from(span)) as u16
}

/// The kernel's ephemeral range's low bound: the ports from it up are those
/// it hands to a bind on port 0 and to an outbound connect.
fn ephemeral_low() -> io::Result<u16> {
    let Ok(stated) = std::fs::read_to_string(RANGE_FILE) else {
        return Ok(EPHEMERAL_LOW);
    };
    stated.split_whitespace().next().and_then(|low| low.parse().ok()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{RANGE_FILE} states no low bound: {stated:?}"),
        )
    })
}

/// The ports `[floor, low)` below an ephemeral range starting at `low`: from
/// [`FLOOR`] where that leaves [`SPAN`] ports, else from [`UNPRIVILEGED`].
fn range_below(low: u16) -> io::Result<(u16, u16)> {
    let floor = if low.saturating_sub(FLOOR) >= SPAN { FLOOR } else { UNPRIVILEGED };
    if low.saturating_sub(floor) < SPAN {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!("the ephemeral range starts at {low}: fewer than {SPAN} unprivileged ports lie below it"),
        ));
    }
    Ok((floor, low))
}

/// The sockets holding the ports picked so far, released when it drops.
#[derive(Default)]
struct Held {
    udp: Vec<UdpSocket>,
    tcp: Vec<TcpListener>,
}

impl Held {
    /// Binds a socket of `kind` to loopback `port` and keeps it; `false` where the bind fails.
    fn hold(&mut self, kind: Kind, port: u16) -> bool {
        let at = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        match kind {
            Kind::Udp => UdpSocket::bind(at).map(|socket| self.udp.push(socket)).is_ok(),
            Kind::Tcp => TcpListener::bind(at).map(|listener| self.tcp.push(listener)).is_ok(),
        }
    }
}

/// An address for each of `kinds`, each the first of `candidate`'s ports
/// that no earlier address took and that binds; every socket is held until
/// the last address is picked.
fn pick(kinds: &[Kind], mut candidate: impl FnMut() -> u16) -> io::Result<Vec<SocketAddr>> {
    let mut held = Held::default();
    let mut picked: Vec<u16> = Vec::with_capacity(kinds.len());
    for &kind in kinds {
        let port = (0..TRIES)
            .find_map(|_| {
                let port = candidate();
                (!picked.contains(&port) && held.hold(kind, port)).then_some(port)
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    format!("no free {kind:?} loopback port in {TRIES} candidates"),
                )
            })?;
        picked.push(port);
    }
    drop(held);
    Ok(picked.into_iter().map(|port| SocketAddr::from((Ipv4Addr::LOCALHOST, port))).collect())
}

#[cfg(test)]
mod tests;
