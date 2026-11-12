//! The loopback datagram preflight a hosted lane runs before it spawns
//! anything: proof that every address the run puts SIP on carries a full-size
//! datagram. A path that fragments without reassembling drops the b-leg INVITE
//! with no error on either side, which reads downstream as a SUT that never
//! dialled.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::Duration;

use crate::{Case, Failure};

/// The datagram size the preflight proves a path carries. A captured INVITE
/// with SDP routinely passes 1500 bytes, so a path that fragments without
/// reassembling swallows the very message the lane exists to deliver.
const PROBE_BYTES: usize = 4096;

/// Every address this run puts SIP on: the case's bound endpoints, plus
/// whatever else the lane names (its own loopback host, the SUT's ingress).
pub fn carries_full_datagrams(case: &Case, also: &[IpAddr]) -> Result<(), Failure> {
    let mut hosts: BTreeSet<IpAddr> = case.endpoint_addrs.values().map(SocketAddr::ip).collect();
    hosts.extend(also.iter().copied());
    for ip in hosts {
        carries_full_datagram(ip)?;
    }
    Ok(())
}

/// One address probed against ITSELF — the route lookup is per destination, so
/// a self-probe answers for every path INTO that address.
fn carries_full_datagram(ip: IpAddr) -> Result<(), Failure> {
    let bind = |what: &str, e: std::io::Error| {
        Failure::Environment(format!("loopback preflight on {ip}: {what}: {e}"))
    };
    let sock = UdpSocket::bind(SocketAddr::new(ip, 0)).map_err(|e| bind("bind", e))?;
    let local = sock.local_addr().map_err(|e| bind("local_addr", e))?;
    sock.set_read_timeout(Some(Duration::from_millis(500))).map_err(|e| bind("timeout", e))?;
    sock.send_to(&vec![b'p'; PROBE_BYTES], local).map_err(|e| bind("send", e))?;
    let mut buf = vec![0u8; PROBE_BYTES * 2];
    match sock.recv_from(&mut buf) {
        Ok((n, _)) if n == PROBE_BYTES => Ok(()),
        Ok((n, _)) => Err(Failure::Environment(format!(
            "loopback preflight on {ip}: a {PROBE_BYTES}-byte datagram came back torn at {n} bytes"
        ))),
        Err(_) => Err(Failure::Environment(format!(
            "loopback preflight on {ip}: a {PROBE_BYTES}-byte datagram never arrived — the path \
             fragments without reassembling, so every SIP message over the link MTU is lost \
             silently. Check `ip route get {ip}`: on a WSL mirrored-networking host 127.0.0.1 \
             leaves `lo` for a 1500-MTU device; state another 127/8 address as lane.loopback_host \
             and in the run-spec's endpoint_addresses"
        ))),
    }
}
