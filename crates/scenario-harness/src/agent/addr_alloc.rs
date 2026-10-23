//! The loopback addresses a harness hands out on its own simulated network:
//! ports 5060, 5070, 5080, … in steps of 10, the SIP default port first,
//! skipping every address the harness already bound. A fresh harness's first
//! three are the canonical `5060`/`5070`/`5080` a two-party-and-SUT scene uses.

use std::cell::RefCell;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// The first port handed out, and the step between two.
const FIRST_PORT: u16 = 5060;
const STEP: u16 = 10;

/// The allocator: the next port to try and the addresses already bound.
pub(super) struct AddrAlloc {
    state: RefCell<(u16, HashSet<SocketAddr>)>,
}

impl Default for AddrAlloc {
    fn default() -> Self {
        AddrAlloc { state: RefCell::new((FIRST_PORT, HashSet::new())) }
    }
}

impl AddrAlloc {
    /// A loopback address no earlier call returned and no bind took.
    pub(super) fn next(&self) -> SocketAddr {
        let mut state = self.state.borrow_mut();
        let (port, taken) = &mut *state;
        loop {
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), *port);
            *port = port.checked_add(STEP).expect("the harness ran out of loopback ports");
            if taken.insert(addr) {
                return addr;
            }
        }
    }

    /// Note `addr` bound, so [`next`](Self::next) never returns it.
    pub(super) fn note_bound(&self, addr: SocketAddr) {
        self.state.borrow_mut().1.insert(addr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_three_are_the_canonical_ports_and_a_bound_address_is_skipped() {
        let alloc = AddrAlloc::default();
        alloc.note_bound("127.0.0.1:5090".parse().unwrap());
        let ports: Vec<u16> = (0..4).map(|_| alloc.next().port()).collect();
        assert_eq!(ports, [5060, 5070, 5080, 5100]);
    }
}
