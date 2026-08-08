//! `referTransfer` — the REFER-driven blind-transfer service. A bridged B leg
//! REFERs; `/call/refer` authorizes; the B2BUA dials the transfer target C on
//! held SDP, realigns C to A's SDP, realigns A to C's answer, then merges A↔C
//! (B is left orphaned; its BYE rides the CORE relay path).
//!
//! - [`seed`] — the CORE_LAYER rules that run BEFORE the transfer slice exists
//!   (intercept the first REFER, reject Replaces / a-leg REFER).
//! - [`machine`] — the phase-gated SERVICE_LAYER rules (ADR-0016 machine) plus
//!   the cursor projection.
//! - [`notify`] — the RFC 3515 sipfrag progress-NOTIFY vocabulary.
//!
//! Per-call state lives on the typed `Call.transfer` slice; rules are stateless
//! `fn`s that read `ctx.call.transfer_state()` and emit `SetTransfer` to
//! advance the phase. An attended transfer (`?Replaces=`) is rejected 501.

mod machine;
mod notify;
mod seed;

pub use machine::{project_cursor, transfer_rules, transfer_service_def};
pub use seed::transfer_seed_rules;

use super::model::{RuleAction, RuleHandleResult};

fn ok(actions: Vec<RuleAction>) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(actions))
}
