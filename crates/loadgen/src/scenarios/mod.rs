//! The load scenarios — fallible reuse of the functional choreography.
//!
//! The portable actor-declared scenario bodies + their trait/runner live in
//! [`scenario_harness::actor`] (the per-call environment in
//! [`scenario_harness::realcall`]) so the SAME flow serves the load fleet AND
//! the in-process functional leak gate. The
//! **tables** that used to live here (`by_id` / `default_scenarios` /
//! `failure_scenarios`) folded into the unified, open shape registry
//! ([`e2e_model::ShapeRegistry`]): each shape is declared ONCE as a
//! [`ShapeDescriptor`](e2e_model::ShapeDescriptor) — id, load attributes
//! (needs-charlie / needs-bob2 / emergency), mix weights, and the load-body
//! factory — and the driver's [`MixEntry`](crate::driver::MixEntry) is built
//! from it (`MixEntry::by_id` / `default_mix` / `failure_mix`). This module
//! re-exports the shared pieces so downstream users reach them unchanged.
//!
//! # Shape-authoring caveat: repeated identical provisionals are invisible
//!
//! (newkahneed-033 ask D2.) A B2BUA presents ONE a-leg early dialog, so when
//! it relays a *second* ringing leg's 180 (a reroute, a sequential fork) the
//! relayed 180 is **byte-identical** to the first — same Via branch, same
//! To-tag. Under `--auto-retransmit` the mux's per-call engine dedups inbound
//! `(branch, status)` pairs (on the wire it IS a retransmission), so the body
//! never sees the repeat; without the engine the duplicate 180 reaches the
//! body but carries nothing to tell the legs apart. Either way **a load body
//! can never observe "ring again" semantics** — do not write a shape that
//! waits for the second 180 (two independent tools, the engine's dedup and
//! `sipflow`'s capture-dedup, once agreed on that wrong conclusion). Assert
//! per-leg ringing on the UAS side (each leg's own 180 send), use
//! `ClientInvite::try_expect_final` on the caller to absorb the relay-timing-
//! dependent 1xx count, and leave "rang twice" assertions to the
//! functional/e2e surface where no absorber sits between the SUT and the UA.

// The per-call context types are re-exported so any downstream user reaches
// them through this module unchanged.
pub use scenario_harness::realcall::{CallCtx, CallEnv, CallScope, ScenarioId};
// The load bodies are ACTOR-declared (per-endpoint reactive actors the runner
// joins); re-export the whole set plus their trait/runner under this module so
// a `scenarios::BasicCall` user resolves. `LoadScenario` is the historic alias
// for the load body's trait (now `ActorScenario`).
pub use scenario_harness::actor::scenarios::{
    AbandonRinging, BasicCall, InviteReject, LongCall, OptionsHold, PrackUpdate, Refer,
    ReferCharlieReject, Reinvite, ReroutingPrack,
};
pub use scenario_harness::actor::{
    run_actor_scenario, ActorCall, ActorScenario, ActorScenario as LoadScenario, Expect,
};

// The unified shape registry + the per-run construction inputs (SUT auth data
// such as the refer key), declared once in the shared axis model.
pub use e2e_model::{LegSpec, ScenarioInputs, ShapeDescriptor, ShapeRegistry};
