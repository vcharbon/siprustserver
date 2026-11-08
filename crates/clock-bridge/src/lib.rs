//! clock-bridge: a paused-clock test talking to a real external process.
//!
//! The work runs on a [`RealSide`], a real-clock runtime on threads of its
//! own, from the instant it is started. Its result is a [`Pending`], handed to
//! the paused-clock side at a virtual instant chosen by the caller
//! ([`Pending::deliver_at`]): virtual time runs on for everything else until
//! that instant, and only then, if the result is still on its way, the paused
//! clock is held for real, up to the side's cap. A result past the cap is an
//! environment fault, never a reply: it lands in the [`FaultLog`] the test's
//! gate reads.
//!
//! Every interaction is one of two shapes: a request the test or the system
//! under test issues (deliver at issue + latency), or an output the test
//! awaits (deliver now). An external process pushing unsolicited input at
//! unpredictable real times has no virtual instant and runs on the real clock.

mod fault_log;
mod mode;
mod pending;
pub mod poison;
mod real_side;

pub use fault_log::{EnvFault, FaultLog};
pub use mode::ClockMode;
pub use pending::{Pending, Undelivered};
pub use real_side::RealSide;
