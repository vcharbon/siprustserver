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
//! Two tiers send the reject, [`build_capacity_reject_503`]: the ingress brake
//! ([`crate::tier1_brake`]) from the level of the last sample, before any
//! transaction exists, and the initial-INVITE admission gate from exact counts,
//! ahead of the CPS bucket.
//!
//! - `probe`: the [`SystemProbe`] read seam and its [`simulated`] pair.
//! - `gate`: [`CapacityGate`], its ceilings, sample, level and tallies.
//! - `reject`: the 503 both tiers send.
//! - `prometheus`: `/metrics` exposition.

mod gate;
mod probe;
mod prometheus;
mod reject;

pub use gate::{BackupBound, Bound, CapacityGate, Level, Occupancy, Tier};
pub use probe::{simulated, SimulatedSystemControl, SimulatedSystemProbe, SystemProbe};
pub use reject::build_capacity_reject_503;

#[cfg(test)]
mod tests;
