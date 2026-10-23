//! The call's side of its admission set (ADR-0040): the recipes that apply
//! the limiter's answers to a call ([`call`], [`report`]) and the slack the
//! worker's admit bound shares with the answer deadlines.

use std::time::Duration;

pub mod call;
pub mod report;

/// How long past the limiter's admit budget the worker waits for any admit
/// before it answers `unavailable`: a client's own timeout fires first and is
/// counted. Well inside the answer deadline's margin (ADR-0039).
pub const ADMIT_SLACK: Duration = Duration::from_millis(100);
