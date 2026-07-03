//! The load scenarios — fallible reuse of the functional choreography.
//!
//! The portable real-call scenarios + the trait + the shared `establish`/
//! `hangup` choreography live in [`scenario_harness::realcall`] so the SAME
//! flow serves the load fleet AND the in-process functional leak gate. The
//! **tables** that used to live here (`by_id` / `default_scenarios` /
//! `failure_scenarios`) folded into the unified, open shape registry
//! ([`e2e_model::ShapeRegistry`]): each shape is declared ONCE as a
//! [`ShapeDescriptor`](e2e_model::ShapeDescriptor) — id, load attributes
//! (needs-charlie / needs-bob2 / emergency), mix weights, and the load-body
//! factory — and the driver's [`MixEntry`](crate::driver::MixEntry) is built
//! from it (`MixEntry::by_id` / `default_mix` / `failure_mix`). This module
//! re-exports the shared pieces so downstream users reach them unchanged.

// The shared real-call trait + choreography (the load alias keeps the historic
// `LoadScenario` name at the call sites). `establish`/`hangup` and the
// per-call context types are re-exported so any downstream user reaches them
// through this module unchanged.
pub use scenario_harness::realcall::{
    establish, establish_100rel, hangup, hangup_on, CallCtx, CallEnv, CallScope,
    RealCallScenario as LoadScenario, ScenarioId,
};
// All scenario bodies live in the shared crate; re-export so a
// `scenarios::BasicCall` user resolves.
pub use scenario_harness::realcall::scenarios::{
    AbandonRinging, BasicCall, InviteReject, LongCall, OptionsHold, PrackUpdate, Refer,
    ReferCharlieReject, Reinvite, ReroutingPrack,
};

// The unified shape registry + the per-run construction inputs (SUT auth data
// such as the refer key), declared once in the shared axis model.
pub use e2e_model::{ScenarioInputs, ShapeDescriptor, ShapeRegistry};
