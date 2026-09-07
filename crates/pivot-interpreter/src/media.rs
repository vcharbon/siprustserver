//! The lane's media plane: what a rewritten SDP body carries
//! (`PCAP2TEST_PIVOT_V3.md` §8.3, tokens `c=addr` and `m=port`).
//!
//! One source answers both tokens — the address every rewritten `c=` line takes
//! and the port every rewritten `m=` line takes — so a replayed offer never
//! advertises an address the lane holds beside a port it does not.
//!
//! A port is BOOKED, never invented: the i-th active `m=` line of a leg draws
//! the i-th port reserved for that leg, and re-rendering that leg's body draws
//! the same one. The lane holds ports without sending media (`legs[].media.rtp`
//! names the lane as the source); nothing here opens a socket.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// The lane's media booking.
pub struct Booking {
    addr: String,
    ports: Mutex<Ports>,
}

struct Ports {
    next: u16,
    held: BTreeMap<(String, usize), u16>,
}

impl Booking {
    /// A booking that writes `addr` into every rewritten `c=` line and hands out
    /// even RTP ports from `base` upward.
    pub fn new(addr: impl Into<String>, base: u16) -> Self {
        Booking {
            addr: addr.into(),
            ports: Mutex::new(Ports { next: base, held: BTreeMap::new() }),
        }
    }

    /// The address a rewritten `c=` line carries.
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// The RTP port the `index`-th active `m=` line of `leg` carries, reserving
    /// it on first ask. `pairs` is the stream's `<port>/<count>` port-pair count
    /// (RFC 4566 §5.14), which widens the reservation and never the port.
    /// Idempotent per `(leg, index)`: a re-rendered body stamps the same port.
    pub fn port(&self, leg: &str, index: usize, pairs: u16) -> u16 {
        let mut ports = self.ports.lock().expect("the media booking is never poisoned");
        if let Some(&held) = ports.held.get(&(leg.to_string(), index)) {
            return held;
        }
        let port = ports.next;
        ports.next = ports.next.saturating_add(2u16.saturating_mul(pairs.max(1)));
        ports.held.insert((leg.to_string(), index), port);
        port
    }

    /// The port already held for the `index`-th active `m=` line of `leg`, if
    /// one is — the evidence a caller needs to assert on a booking it did not
    /// make itself.
    pub fn held(&self, leg: &str, index: usize) -> Option<u16> {
        self.ports
            .lock()
            .expect("the media booking is never poisoned")
            .held
            .get(&(leg.to_string(), index))
            .copied()
    }
}

impl std::fmt::Debug for Booking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Booking").field("addr", &self.addr).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_booking_is_idempotent_per_leg_and_stream_and_steps_by_a_port_pair() {
        let booking = Booking::new("127.0.0.1", 40000);
        assert_eq!(booking.addr(), "127.0.0.1");
        assert_eq!(booking.held("A", 0), None, "nothing is held before it is asked for");
        let a0 = booking.port("A", 0, 1);
        assert_eq!(a0, 40000);
        assert_eq!(booking.port("A", 0, 1), a0, "re-rendering a leg stamps the port it holds");
        assert_eq!(booking.port("A", 1, 1), 40002, "a second stream takes its own pair");
        assert_eq!(booking.port("B", 0, 1), 40004, "another leg takes its own pair");
        assert_eq!(booking.held("A", 1), Some(40002));
    }

    /// A `<port>/<count>` stream reserves `count` contiguous pairs, so the next
    /// stream starts past all of them (RFC 4566 §5.14).
    #[test]
    fn a_multi_pair_stream_reserves_every_pair_it_names() {
        let booking = Booking::new("127.0.0.1", 40000);
        assert_eq!(booking.port("A", 0, 2), 40000);
        assert_eq!(booking.port("A", 1, 1), 40004, "the /2 stream held 40000 and 40002");
    }
}
