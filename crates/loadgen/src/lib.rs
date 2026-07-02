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
//! - [`scenarios`] — the [`LoadScenario`](scenarios::LoadScenario) trait + the
//!   v1 set, fallible ports of the `b2bua-harness` functional tests.
//! - [`driver`] — the CPS governor + max-in-flight semaphore + per-call
//!   `catch_unwind` boundary + [`scope`]-based teardown.
//! - [`report`] — bounded-memory counters, latency histograms, sampling gate,
//!   Prometheus text, on-disk report.
//! - [`class`] — result classification.
//!
//! [`Agent`]: scenario_harness::Agent

pub mod chaos;
pub mod class;
pub mod ctx;
pub mod driver;
pub mod mux;
pub mod report;
pub mod scenarios;
pub mod scope;

pub use chaos::{ChaosLog, ChaosTag};
pub use class::{CallOutcome, ResultClass};
pub use ctx::{CallCtx, CallEnv};
pub use driver::{serve_metrics, CallConfig, CallTuning, Driver, DriverCfg, MuxTransport};
pub use mux::{CallRouting, Correlation, EndpointSpec, LegInfo, LegPicker, MuxCore, Role};
pub use report::{Reporter, ReporterCfg};
pub use scenarios::{by_id, default_scenarios, failure_scenarios, LoadScenario, ScenarioId};
pub use scope::CallScope;
