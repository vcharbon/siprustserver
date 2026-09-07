//! The basic-B2BUA default rule set (CORE_LAYER): the bridged-call lifecycle
//! INVITE → 18x → 200 → ACK → in-dialog → BYE, plus CANCEL, b-leg failure,
//! failover resolution, and the housekeeping timers.
//!
//! - [`compose`] — which rule families make up the default set, in which
//!   priority order, and the compose-time opt-out seam ([`ComposeOptions`]).
//! - [`core_rules`] — the CORE_LAYER rules themselves, one exhaustive
//!   registration list.
//! - [`route_fold`] — the shared route-shaped-payload parser + parity actions
//!   both async route folds (`call-failure-result`, `call-release-result`)
//!   apply identically.
//!
//! The 18x-policy and transfer rule families do NOT live here — see
//! `rules::relay_first_18x`, `rules::promote_pem`, `rules::refer_transfer`.

mod compose;
mod core_rules;
mod route_fold;

pub use compose::{default_rules, default_rules_with, ComposeOptions};
pub(crate) use core_rules::unacked_2xx_give_up_actions;
pub(crate) use route_fold::{
    fold_lands_on_going_away_call, parse_route_fold, route_fold_parity_actions,
};
