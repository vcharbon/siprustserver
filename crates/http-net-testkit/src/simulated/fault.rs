//! The faults a destination of the simulated fabric meets: per request
//! ([`Behaviour`]) and as applied ([`Fault`]).

use std::net::SocketAddr;

#[cfg(doc)]
use super::SimulatedHttpNetwork;

/// What a destination does with one request (see the module doc).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Behaviour {
    /// The service answers.
    Answer,
    /// The connection is refused at once.
    Refuse,
    /// The connect never completes; the connect timeout ends it.
    Blackhole,
    /// Sent and never answered until a [`Fault::Resume`].
    Silent,
    /// The service answers; the body never completes.
    StallBody,
    /// The connection is reset after `after_ms`.
    Reset {
        /// Delay before the reset, in milliseconds.
        after_ms: u64,
    },
    /// The connection is closed after the request with no byte back.
    Eof,
}

/// A dst-keyed fault. Apply with [`SimulatedHttpNetwork::apply_fault`].
#[derive(Clone, Debug)]
pub enum Fault {
    /// Raise the per-hop transit delay (ms, coerced to `>= 1`) for `dst`.
    Delay {
        /// The destination whose delivery slows.
        dst: SocketAddr,
        /// New transit delay in milliseconds.
        ms: u64,
    },
    /// Hang requests to `dst` ([`Behaviour::Silent`]): they never complete
    /// until a [`Fault::Resume`] (or the caller times out).
    Stall {
        /// The destination to stall.
        dst: SocketAddr,
    },
    /// Clear any fault and script on `dst` and wake stalled requests.
    Resume {
        /// The destination to resume.
        dst: SocketAddr,
    },
    /// Refuse requests to `dst` ([`Behaviour::Refuse`]).
    Cut {
        /// The destination to cut.
        dst: SocketAddr,
    },
    /// Reset requests to `dst` after `ms` ([`Behaviour::Reset`]).
    ErrorAfter {
        /// The destination that resets.
        dst: SocketAddr,
        /// Delay before the error fires, in milliseconds.
        ms: u64,
    },
    /// Give every request to `dst` this standing behaviour.
    Behave {
        /// The destination.
        dst: SocketAddr,
        /// What it does with each request.
        behaviour: Behaviour,
    },
    /// Give the next requests to `dst` these behaviours, one each, before
    /// the standing one.
    Script {
        /// The destination.
        dst: SocketAddr,
        /// One behaviour per request, in order.
        steps: Vec<Behaviour>,
    },
    /// `dst` closes every connection open to it now; new ones are taken as
    /// before.
    Close {
        /// The backend.
        dst: SocketAddr,
    },
}

impl Behaviour {
    /// Whether a backend standing on it has closed its open connections.
    pub(super) fn ends_connections(&self) -> bool {
        matches!(self, Behaviour::Refuse | Behaviour::Reset { .. } | Behaviour::Eof)
    }
}
