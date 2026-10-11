//! [`Fault`] — the faults a [`SimulatedReplicationNetwork`] injects.

use std::net::SocketAddr;

#[cfg(doc)]
use super::super::{ConnectError, SendError};
#[cfg(doc)]
use super::SimulatedReplicationNetwork;

/// A directed connection fault, keyed by `(src, dst)` direction or by endpoint.
///
/// Set on the [`SimulatedReplicationNetwork`] builder before the fabric is
/// shared, or applied to a live fabric. `delay`/`stall`/`cut` are **directed**
/// (apply to `src → dst`); `partition`/`heal` are **bidirectional** (both
/// directions between two endpoints). `reconnect` is not a fault — it is just
/// a fresh `connect` succeeding once any `cut`/partition on the pair is cleared.
///
/// A directed fault whose `src` and `dst` are both **declared endpoints**
/// ([`declare_endpoint`](SimulatedReplicationNetwork::declare_endpoint)) is a
/// node fault: it applies to every stream between the two nodes, present and
/// future, in that direction — the streams either node's puller opened from an
/// ephemeral local included. Any other pair is one wire direction, literally.
#[derive(Clone, Debug)]
pub enum Fault {
    /// Raise the transit delay `src → dst` (coerced to `>= 1 ms`). A frame
    /// stamped after the fault (the delivery actor stamps at its next poll)
    /// waits the new delay; one already stamped keeps its deadline.
    Delay {
        /// Source endpoint of the directed connection.
        src: SocketAddr,
        /// Destination endpoint of the directed connection.
        dst: SocketAddr,
        /// New transit delay in milliseconds.
        ms: u64,
    },
    /// Pause delivery on `src → dst`: the staging queue grows, nothing is
    /// delivered, until a matching [`Fault::Resume`].
    Stall {
        /// Source endpoint.
        src: SocketAddr,
        /// Destination endpoint.
        dst: SocketAddr,
    },
    /// Resume a stalled direction: buffered frames flush in order.
    Resume {
        /// Source endpoint.
        src: SocketAddr,
        /// Destination endpoint.
        dst: SocketAddr,
    },
    /// Cut `src → dst` now: in-flight + future sends on live connections of
    /// that direction fail; their `recv` yields `None`. Between declared
    /// endpoints the cut outlives the streams it closes — a stream reopened
    /// under it dies as soon as it is attributed — until a [`Fault::Heal`].
    Cut {
        /// Source endpoint.
        src: SocketAddr,
        /// Destination endpoint.
        dst: SocketAddr,
    },
    /// Partition two endpoints: hold delivery in **both** directions (frames
    /// buffer in order and flush on [`Fault::Heal`]) and refuse new `connect`s
    /// between them until then.
    Partition {
        /// One endpoint.
        a: SocketAddr,
        /// The other endpoint.
        b: SocketAddr,
    },
    /// Heal a partition: clear the block so a fresh `connect` succeeds, and
    /// clear every node fault between the two endpoints, both directions, and
    /// a [`Fault::BlackHole`] on either.
    Heal {
        /// One endpoint.
        a: SocketAddr,
        /// The other endpoint.
        b: SocketAddr,
    },
    /// Arm buffer-overflow → drop-subscriber on `src → dst`: when the bounded
    /// inbound buffer is full, the delivery actor cuts the connection instead
    /// of awaiting space (the "buffer-full → drop subscriber → reconnect"
    /// scenario).
    DropOnOverflow {
        /// Source endpoint.
        src: SocketAddr,
        /// Destination endpoint.
        dst: SocketAddr,
    },
    /// **Black-hole** `src → dst`: the peer stops pulling, so the application
    /// `send` BLOCKS once the in-flight window is full — modelling a TCP sender
    /// stuck on a full socket buffer with a dead reader. No error, no close:
    /// `send` simply never completes until the peer drains (its `recv` releases a
    /// window slot), the block is cleared by [`Fault::Resume`], or the direction
    /// is later [`Cut`](Fault::Cut)/[`ErrorAfter`](Fault::ErrorAfter). This is the
    /// half-open / hung-peer case that a clean `Cut` (immediate close) does not
    /// cover.
    Block {
        /// Source endpoint.
        src: SocketAddr,
        /// Destination endpoint.
        dst: SocketAddr,
    },
    /// **Network error after a delay** on `src → dst`: after `ms` elapse, an
    /// established `send` returns [`SendError::Io`], the delivery actor tears the
    /// inbound down (`recv` → `None`), and a fresh [`connect_from`] on this pair
    /// is rejected with [`ConnectError::Io`] — modelling a reset (ECONNRESET)
    /// some time into the connection's life, distinct from a clean `Cut`.
    ///
    /// [`connect_from`]: SimulatedReplicationNetwork::connect_from
    ErrorAfter {
        /// Source endpoint.
        src: SocketAddr,
        /// Destination endpoint.
        dst: SocketAddr,
        /// Delay before the error fires, in milliseconds.
        ms: u64,
    },
    /// **Black-hole** the address `dst`: every `connect` to it from now on
    /// waits with no answer, neither refused nor established, until a
    /// [`Fault::Heal`] names `dst` (then it proceeds as an ordinary connect).
    /// Streams already open to `dst` are untouched.
    BlackHole {
        /// The address that answers no connect.
        dst: SocketAddr,
    },
}
