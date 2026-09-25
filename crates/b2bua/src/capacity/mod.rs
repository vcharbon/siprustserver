//! Worker-side memory admission (ADR-0037). Its one goal on the wire: **refuse
//! a new call with a 503 before the worker's memory runs out**.
//!
//! Three quantities are bounded: live calls (takeover copies included), live
//! SIP transactions, and process RSS. Each has a `normal` ceiling, at which a
//! new non-emergency call is refused, and an `emergency` ceiling, at which
//! every new call is. The backup replicas this node holds for its peers have
//! their own count and RSS ceilings, at which a replica of a call not yet held
//! is not stored. In-dialog traffic is never refused.
//!
//! The initial-INVITE admission gate sends the reject,
//! [`build_capacity_reject_503`], from exact counts, ahead of the CPS bucket and
//! behind the INVITE server transaction, which absorbs retransmissions of an
//! admitted INVITE.
//!
//! - `probe`: the [`SystemProbe`] read seam and its [`simulated`] pair.
//! - `gate`: [`CapacityGate`], its ceilings, sample, level and tallies.
//! - `reject`: the 503 the gate answers with.
//! - `prometheus`: `/metrics` exposition.

mod gate;
mod probe;
mod prometheus;
mod reject;

pub use gate::{BackupBound, Bound, CapacityGate, Level, Occupancy};
pub use probe::{simulated, SimulatedSystemControl, SimulatedSystemProbe, SystemProbe};
pub use reject::build_capacity_reject_503;

#[cfg(test)]
mod tests;
