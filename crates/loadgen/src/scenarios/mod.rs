//! The load scenarios — fallible reuse of the functional choreography.
//!
//! The portable real-call scenarios + the trait + the shared `establish`/
//! `hangup` choreography now live in [`scenario_harness::realcall`] so the SAME
//! flow serves the load fleet AND the in-process functional leak gate. This
//! module is the **load registry**: it re-exports the shared pieces and the
//! migrated scenario structs and owns the weight/`by_id` tables the driver and
//! CLI consume.

use std::sync::Arc;

// The shared real-call trait + choreography (the load alias keeps the historic
// `LoadScenario` name at the call sites). `establish`/`hangup`/`AsEmergency` and
// the per-call context types are re-exported so the registries below and any
// downstream user reach them through this module unchanged.
pub use scenario_harness::realcall::{
    establish, hangup, AsEmergency, CallCtx, CallEnv, CallScope,
    RealCallScenario as LoadScenario, ScenarioId,
};
// All scenarios now live in the shared crate; re-export so the registries below
// (and any `scenarios::BasicCall` user) resolve.
pub use scenario_harness::realcall::scenarios::{
    AbandonRinging, BasicCall, InviteReject, LongCall, OptionsHold, PrackUpdate, Refer,
    ReferCharlieReject, Reinvite,
};

/// All v1 scenarios with default weights (basic-heavy, like real traffic).
pub fn default_scenarios() -> Vec<(Arc<dyn LoadScenario>, f64)> {
    vec![
        (Arc::new(BasicCall), 4.0),
        (Arc::new(Reinvite), 2.0),
        (Arc::new(Refer), 1.0),
        (Arc::new(OptionsHold), 1.0),
    ]
}

/// Resolve a scenario by id (for CLI `--scenario name=weight`). The `*_em`
/// variants are emergency (Resource-Priority `esnet.0`) calls of the same flow.
pub fn by_id(id: &str) -> Option<Arc<dyn LoadScenario>> {
    match id {
        "basic_call" => Some(Arc::new(BasicCall)),
        "reinvite" => Some(Arc::new(Reinvite)),
        "refer" => Some(Arc::new(Refer)),
        "options_hold" => Some(Arc::new(OptionsHold)),
        "long_call" => Some(Arc::new(LongCall)),
        "prack_update" => Some(Arc::new(PrackUpdate)),
        "basic_call_em" => Some(AsEmergency::wrap("basic_call_em", Arc::new(BasicCall))),
        "reinvite_em" => Some(AsEmergency::wrap("reinvite_em", Arc::new(Reinvite))),
        "invite_reject" => Some(Arc::new(InviteReject)),
        "abandon_ringing" => Some(Arc::new(AbandonRinging)),
        "refer_charlie_reject" => Some(Arc::new(ReferCharlieReject)),
        _ => None,
    }
}

/// The voluntarily-failing scenarios (one per post-call-cleanup teardown path),
/// for a no-leak cleanup-coverage test without an endurance run.
pub fn failure_scenarios() -> Vec<(Arc<dyn LoadScenario>, f64)> {
    vec![
        (Arc::new(InviteReject), 1.0),
        (Arc::new(AbandonRinging), 1.0),
        (Arc::new(ReferCharlieReject), 1.0),
    ]
}
