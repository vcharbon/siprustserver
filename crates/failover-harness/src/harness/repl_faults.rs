//! The replication-fabric fault verbs of [`FailoverHarness`]: each names
//! workers by ordinal and reaches every stream between their current
//! incarnations.

use std::net::SocketAddr;

use repl_net::transport::Fault;

use super::FailoverHarness;

impl FailoverHarness {
    /// The replication listen address of `ordinal`'s current incarnation — the
    /// one the cluster's resolver hands out, so a fault names the node that is
    /// serving now, a replacement included.
    fn repl_listen(&self, ordinal: &str) -> SocketAddr {
        self.repl_resolver.lock().unwrap().get(ordinal).copied().unwrap_or_else(|| {
            panic!("{ordinal} has no replication listen address in the cluster's resolver")
        })
    }

    /// Partition two workers on the repl fabric, both directions and for every
    /// stream between them — the ones their pullers already opened, and any they
    /// try to open while the cut lasts. The fabric attributes each stream to
    /// the node that opened it (at `connect`, through the node's own handle)
    /// and holds delivery on the owner pair; a connect across the cut is
    /// refused, so a reconnect from a fresh ephemeral local does not slip
    /// through and the puller never reads the peer as reached. A held
    /// direction buffers in order and flushes on [`heal`](Self::heal). Names
    /// each ordinal's current incarnation. Marker.
    pub fn partition(&mut self, a: &str, b: &str) {
        let (aa, ba) = (self.repl_listen(a), self.repl_listen(b));
        self.repl_sim.apply_fault(Fault::Partition { a: aa, b: ba });
        self.mark(a, Some(b), "partition", "");
    }

    /// Delay delivery by `ms` on every replication stream from `from` to `to`,
    /// present and future, in that direction — the frames `from`'s listener
    /// serves down the streams `to`'s pullers opened, and the ones `from`'s own
    /// pullers send up to `to`. A flush `from` writes lands on `to` no earlier
    /// than `ms` after it left; a stream `to` reopens later inherits the delay.
    /// Names each ordinal's current incarnation; lifted by
    /// [`heal`](Self::heal). Marker.
    pub fn delay(&mut self, from: &str, to: &str, ms: u64) {
        let (src, dst) = (self.repl_listen(from), self.repl_listen(to));
        self.repl_sim.apply_fault(Fault::Delay { src, dst, ms });
        self.mark(from, Some(to), "delay", &format!("{ms}ms on {src} → {dst}"));
    }

    /// Hold delivery on every replication stream from `from` to `to`, present
    /// and future, in that direction: nothing `from` sends reaches `to`, and
    /// nothing closes — a silent peer. What is held flushes in order on
    /// [`resume`](Self::resume) or [`heal`](Self::heal). Names each ordinal's
    /// current incarnation. Marker.
    pub fn stall(&mut self, from: &str, to: &str) {
        let (src, dst) = (self.repl_listen(from), self.repl_listen(to));
        self.repl_sim.apply_fault(Fault::Stall { src, dst });
        self.mark(from, Some(to), "stall", &format!("{src} → {dst} held"));
    }

    /// Lift a [`stall`](Self::stall) from `from` to `to`: every held stream
    /// still open flushes in order. Marker.
    pub fn resume(&mut self, from: &str, to: &str) {
        let (src, dst) = (self.repl_listen(from), self.repl_listen(to));
        self.repl_sim.apply_fault(Fault::Resume { src, dst });
        self.mark(from, Some(to), "resume", &format!("{src} → {dst} flowing"));
    }

    /// Heal the repl fabric between two workers: unblock `connect`, lift every
    /// directed fault between them ([`delay`](Self::delay) included) and
    /// release every held stream, which flushes what buffered while the cut
    /// lasted, in order. Names each ordinal's current incarnation. Marker.
    pub fn heal(&mut self, a: &str, b: &str) {
        let (aa, ba) = (self.repl_listen(a), self.repl_listen(b));
        self.repl_sim.apply_fault(Fault::Heal { a: aa, b: ba });
        self.mark(a, Some(b), "heal", "");
    }
}
