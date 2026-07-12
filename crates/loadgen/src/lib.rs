//! `loadgen` — a SIP load generator that reuses the functional
//! `scenario-harness` choreography as a partial SIPp substitute.
//!
//! It mixes basic load (the kind SIPp does well) with complex flows SIPp can't
//! easily express (REFER blind transfer, re-INVITE, OPTIONS-keepalive long hold)
//! against the **real** cluster, with a managed call rate, bounded memory, and
//! dual reporting: a live Prometheus `/metrics` surface plus an on-disk report
//! with bounded per-`(scenario × result-class)` callflow samples (including OK,
//! so OK vs failing flows are comparable).
//!
//! # How it stays cheap
//!
//! The reusable call logic lives in [`scenario_harness`]'s `Send` [`Agent`]
//! choreography (`try_*` fallible methods + best-effort teardown), bound through
//! the `Send` [`scenario_harness::AgentBinder`] — NOT the `!Send` `Harness`. So
//! every call is an ordinary `tokio` task on a shared multi-threaded runtime
//! (thousands concurrent, flat memory), and *recording* is a per-call opt-in
//! decided by the [`report::SamplingGate`](report)-backed
//! [`Reporter::should_record`](report::Reporter::should_record) — no OS thread
//! per call, recorded or not.
//!
//! # Pieces
//!
//! - [`scenarios`] — re-exports the shared [`LoadScenario`](scenarios::LoadScenario)
//!   trait, the scenario bodies (`scenario_harness::realcall`), and the unified
//!   open shape registry ([`ShapeRegistry`](e2e_model::ShapeRegistry)) whose
//!   per-shape [`ShapeDescriptor`](e2e_model::ShapeDescriptor)s the driver's
//!   [`MixEntry`](driver::MixEntry) is built from.
//! - [`driver`] — the CPS governor + max-in-flight semaphore + per-call
//!   `catch_unwind` boundary + [`scope`]-based teardown.
//! - [`app`] — the whole CLI application ([`app::Args`] + [`app::run`]) behind
//!   an injectable [`ShapeRegistry`](e2e_model::ShapeRegistry), so a
//!   third-party load bin is a one-liner passing its own composed registry
//!   (the shipped bin passes `with_defaults()`).
//! - [`report`] — bounded-memory counters, latency histograms, sampling gate,
//!   Prometheus text, on-disk report.
//! - [`class`] — result classification.
//!
//! [`Agent`]: scenario_harness::Agent

pub mod app;
pub mod case;
pub mod chaos;
pub mod class;
pub mod ctx;
pub mod driver;
pub mod mux;
pub mod rate;
pub mod report;
pub mod scenarios;
pub mod scope;

pub use case::{DwellOverrides, LoadCase, ResolvedCall};
pub use chaos::{ChaosLog, ChaosTag};
pub use class::{CallOutcome, ResultClass};
pub use ctx::{CallCtx, CallEnv, CoreIdentity, CorrelationStamp};
pub use driver::{serve_metrics, CallConfig, CallTuning, Driver, DriverCfg, MixEntry, MuxTransport};
pub use mux::{
    labelled_prefix_leg_picker, prefix_leg_picker, CallRouting, Correlation, EndpointSpec,
    DropDir, LegInfo, LegPicker, MuxCore, Role, TargetedDrop,
};
pub use rate::{Governor, RateHandle};
pub use report::{Reporter, ReporterCfg};
// The machine-readable run index (`load-result.json`) the reporter writes — the
// axis data model shared with the e2e website. Re-exported so the CLI/tests use
// one path (`loadgen::LoadRunIndex`) without a direct e2e-model dependency.
pub use e2e_model::{
    Canaries, CheckSummaryRow, CheckpointRow, CountRow, LatencyRow, LoadRunIndex, LoadRunMeta,
    SampleGroup,
};
pub use scenarios::{
    LegSpec, LoadScenario, ScenarioId, ScenarioInputs, ShapeDescriptor, ShapeRegistry,
};
pub use scope::CallScope;
// The environment axis shared with the e2e framework: the egress policy a run's
// `CallConfig`/`CallEnv` carries (authored via `e2e_model::EndpointConfig.egress`,
// domain types hosted in `scenario_harness::egress`).
pub use scenario_harness::egress::{ApiCall, CalleeTarget, EgressPolicy, EgressRewrite};
