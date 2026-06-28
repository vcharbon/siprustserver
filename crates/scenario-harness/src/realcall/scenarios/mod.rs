//! The portable real-call scenario implementations. Each drives one full call
//! against the SUT via the fallible `try_*` surface and is reusable by both the
//! load generator and the in-process functional leak gate.
//!
//! The full set now lives here: the happy-path flows (basic_call, reinvite,
//! refer, options_hold, long_call) and the voluntarily-failing cases (failures).

pub mod basic_call;
pub mod failures;
pub mod long_call;
pub mod options_hold;
pub mod refer;
pub mod reinvite;

pub use basic_call::BasicCall;
pub use failures::{AbandonRinging, InviteReject, ReferCharlieReject};
pub use long_call::LongCall;
pub use options_hold::OptionsHold;
pub use refer::Refer;
pub use reinvite::Reinvite;
