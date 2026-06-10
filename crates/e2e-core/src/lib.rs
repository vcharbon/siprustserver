//! E2E test-management run-core (ADR-0018).
//!
//! Holds the compiled **Callflow shapes** and **Infra shapes** and the seam that
//! runs the *same* scenario over a fake (simulated, paused-clock, in-process
//! LB+b2bua SUT) or a real (external kind cluster) Infra shape. Fronted later by
//! the web site and the CI CLI.

pub mod checks;
pub mod infra;
pub mod model;
pub mod result;
pub mod run;
pub mod shape;
pub mod shapes;

pub use checks::{Bindings, CheckVerdict};
pub use result::{CampaignIndex, CellId, CellSummary, RunResult};
pub use run::{CampaignResult, CampaignSpec, JobHandle, JobStatus, load_spec, run_blocking, spawn_job};
pub use infra::{
    EndpointConfig, FakeLsbcB2bua, InfraKind, InfraRuntime, InfraShape, RealLoopbackDirect,
};
pub use model::{Campaign, Check, CheckBlock, CheckOp, CheckSet, ModelError, TestCase};
pub use shape::{Anchor, CallflowShape, Input};
pub use shapes::BasicCall;
