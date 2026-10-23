//! The basic-B2BUA default rule set (CORE_LAYER): the bridged-call lifecycle
//! INVITE → 18x → 200 → ACK → in-dialog → BYE, plus CANCEL, b-leg failure,
//! failover resolution, and the housekeeping timers.
//!
//! - `compose` — which rule families make up the default set, in which
//!   priority order, and the compose-time opt-out seam ([`ComposeOptions`]).
//! - `core_rules` — the CORE_LAYER rules themselves, one exhaustive
//!   registration list.
//! - `failover_rules` — the CORE_LAYER failover resolution the list
//!   registers in its place: one rule per `/call/failure` outcome.
//! - `own_request_timeouts` — the timeouts of requests this stack originated
//!   itself that end what the request was for, not the call.
//! - `reaper_verdicts` — the bodies of the rules that handle the call
//!   reaper's verdicts (ADR-0020).
//! - `route_fold` — the shared route-shaped-payload parser + parity actions
//!   both async route folds (`call-failure-result`, `call-release-result`)
//!   apply identically.
//!
//! The 18x-policy and transfer rule families do NOT live here — see
//! `rules::relay_first_18x`, `rules::promote_pem`, `rules::refer_transfer`.

mod compose;
mod core_rules;
mod failover_rules;
mod own_request_timeouts;
mod reaper_verdicts;
mod route_fold;

pub use compose::{default_rules, default_rules_with, ComposeOptions};
pub(crate) use core_rules::{still_owed_bare_ack, unacked_2xx_give_up_actions};
pub use route_fold::{
    fold_lands_on_going_away_call, parse_header_updates, parse_label, parse_route_fold,
    parse_service_ext, route_fold_parity_actions,
};
