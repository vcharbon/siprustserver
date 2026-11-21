//! **callfuzz**: a seeded call-flow fuzzer over the actor engine.
//!
//! Each call draws from its own seed ([`call_seed`]: the run's seed hashed
//! with the call's token), so the calls of a run differ and one call's seed
//! replays it alone; a run reproduces whole only where its tokens repeat. A
//! drawn call is any compiled [`ActorCall`]; the
//! fuzzer changes it in RFC-legal ways only, unless the caller opts in:
//!
//! - [`perturb`]: jittered goal dwells (new crossings), hostile but legal
//!   INVITE headers under a size budget, INFOs with odd bodies, the offer in
//!   the INVITE or not and descriptions framed in multipart bodies
//!   ([`media`]);
//! - [`hold_callees`] / [`inject_cancel`]: a plain answering callee held, with
//!   drawn forks, 100rel and a winner that may never have rung, and a caller
//!   CANCEL at a drawn moment;
//! - [`draw_trains`]: per-fork provisional trains (RFC 3262 §3) interleaved
//!   across forks, PRACKs held, finals racing them;
//! - [`FuzzWrap`]: all of the above over any [`ActorScenario`], under a
//!   [`Policy`] the caller supplies per shape.
//!
//! A call accepts the endings its draw allows: one that drew a caller CANCEL
//! accepts the CANCEL races ([`CANCEL_ENDINGS`]), any other keeps the
//! scenario's own. The verdict is that ending (the record's
//! `producer_verdict`), the RFC audit, the reap and the records. [`seal`]
//! stamps the seed into each call's Call-ID and records the draw; [`join()`]
//! joins a run's `call-outcome.v1` records back to their draws.
//!
//! [`ActorCall`]: scenario_harness::actor::ActorCall
//! [`ActorScenario`]: scenario_harness::actor::ActorScenario

use scenario_harness::actor::Branch;

mod config;
pub mod held;
pub mod holds;
pub mod join;
pub mod media;
pub mod payload;
mod perturb;
mod rng;
pub mod stamp;
pub mod train;
mod wrap;

pub use config::FuzzConfig;
pub use held::{hold_callees, inject_cancel, offer_100rel};
pub use join::{join, Joined};
pub use perturb::perturb;
pub use rng::{call_seed, Rng};
pub use stamp::{drawn, seal, seed_of_call_id, take_drawn, Drawn};
pub use train::draw_trains;
pub use wrap::{wrapped_id, wrapped_shapes, FuzzWrap, Policy};

/// Every ending a call whose caller CANCELs its answering callee may legally
/// reach (RFC 3261 §9): the 487, the 2xx crossing the CANCEL, or the 2xx
/// before it left.
pub const CANCEL_ENDINGS: &[Branch] =
    &[Branch::ABANDONED, Branch::ANSWERED_DESPITE_CANCEL, Branch::ANSWERED_BEFORE_CANCEL];

#[cfg(test)]
mod tests;
