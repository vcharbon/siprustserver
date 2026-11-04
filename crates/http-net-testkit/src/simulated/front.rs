//! A balanced address: one address in front of several backends, a backend
//! chosen for each new connection, as a layer-4 load balancer (a virtual
//! service address, a NAT rule set) chooses one per TCP connection and never
//! per request.

use std::net::SocketAddr;

#[cfg(doc)]
use super::SimulatedHttpNetwork;

/// How a balanced address ([`SimulatedHttpNetwork::balance`]) chooses the
/// backend of a new connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    /// The members in turn.
    RoundRobin,
    /// A uniform choice from a pseudo-random sequence of this seed: what a
    /// probabilistic balancer does, the same sequence on every run.
    Seeded(u64),
}

/// One balanced address's members and its choice.
pub(super) struct Front {
    members: Vec<SocketAddr>,
    pick: Pick,
    /// The round-robin counter, or the pseudo-random state.
    state: u64,
}

impl Front {
    pub(super) fn new(members: Vec<SocketAddr>, pick: Pick) -> Self {
        let state = match pick {
            Pick::RoundRobin => 0,
            Pick::Seeded(seed) => seed,
        };
        Self { members, pick, state }
    }

    /// Replaces the members; the choice's sequence goes on.
    pub(super) fn set_members(&mut self, members: Vec<SocketAddr>) {
        self.members = members;
    }

    /// The backend of the next new connection; `None` with no member.
    pub(super) fn next(&mut self) -> Option<SocketAddr> {
        let n = u64::try_from(self.members.len()).ok().filter(|n| *n > 0)?;
        let draw = match self.pick {
            Pick::RoundRobin => {
                let i = self.state;
                self.state = self.state.wrapping_add(1);
                i
            }
            Pick::Seeded(_) => splitmix64(&mut self.state),
        };
        let i = usize::try_from(draw % n).ok()?;
        self.members.get(i).copied()
    }
}

/// One step of SplitMix64: a well-spread 64-bit sequence from any state.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addrs() -> Vec<SocketAddr> {
        vec!["10.0.0.1:80".parse().unwrap(), "10.0.0.2:80".parse().unwrap()]
    }

    #[test]
    fn round_robin_takes_the_members_in_turn() {
        let mut f = Front::new(addrs(), Pick::RoundRobin);
        let got: Vec<_> = (0..4).map(|_| f.next().unwrap()).collect();
        assert_eq!(got, [addrs()[0], addrs()[1], addrs()[0], addrs()[1]]);
    }

    #[test]
    fn a_seed_gives_the_same_spread_sequence_every_time() {
        let draw = || {
            let mut f = Front::new(addrs(), Pick::Seeded(7));
            (0..1000).map(|_| f.next().unwrap()).collect::<Vec<_>>()
        };
        let a = draw();
        assert_eq!(a, draw());
        let first = a.iter().filter(|x| **x == addrs()[0]).count();
        assert!((400..600).contains(&first), "{first} of 1000 on the first member");
    }

    #[test]
    fn no_member_chooses_none() {
        let mut f = Front::new(addrs(), Pick::RoundRobin);
        f.set_members(Vec::new());
        assert_eq!(f.next(), None);
    }
}
