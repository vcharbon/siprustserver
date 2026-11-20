//! The **axis data model** of the e2e documents (ADR-0018/0019 and the loadgen
//! fusion): one dependency-light crate holding the authored JSON documents
//! (Test case / Check set / Endpoint config / Load profile) with their loaders
//! and load-time compatibility validation, the canonical Message-anchor
//! vocabulary, the layout-owned egress-rewrite model, and the post-call check
//! evaluator.
//!
//! Deliberately light: NO SUT crates (b2bua, sip-proxy, call, sip-txn, media)
//! — only serde/schemars/regex plus the message/recording surface the
//! evaluator reads (sip-message, sip-net, scenario-harness). A shape is known
//! here by [`shape::ShapeSpec`]: the load-time metadata slice `validate_case`
//! consumes without knowing how the shape runs.

pub mod bindings;
pub mod checks;
pub mod egress;
pub mod endpoint;
pub mod loadprofile;
pub mod loadrun;
pub mod matrix;
pub mod model;
pub mod registry;
pub mod shape;

pub use bindings::{validate_bindings, BindingMode, BindingPool, BindingResolver, ResolvedBinding};
pub use checks::{
    all_passed, evaluate_blocks, evaluate_blocks_over, evaluate_case, evaluate_case_over, Bindings,
    CheckVerdict,
};
pub use egress::{
    ApiCall, ApiCallDestination, ApiCallRoute, CalleeTarget, EgressPolicy, EgressRewrite,
};
pub use endpoint::{EgressPolicySpec, EndpointConfig};
pub use loadprofile::{LoadProfile, MixSpec, Robustness};
pub use loadrun::{
    Canaries, CheckSummaryRow, CheckpointRow, ClassCount, CountRow, FailureRow, JudgedBasis,
    JudgedShare, Ladder, LadderArrow, LatencyRow, LoadRunIndex, LoadRunMeta, Progress, RateEvent,
    RateEventKind, RttRow, SampleGroup, Sampling, ScenarioRecording, SeriesPoint,
};
pub use model::{
    collect_case_blocks, load_check_set, load_check_sets, load_endpoint_config, load_load_profile,
    load_test_case, schemas, validate_case, Check, CheckBlock, CheckOp, CheckSet, Input,
    ModelError, TestCase,
};
pub use registry::{
    intern, intern_legs, intern_prefixes, LegSpec, LoadFactory, Scenario, ScenarioInputs,
    ShapeDescriptor, ShapeRegistry, ShapeSource,
};
pub use shape::{Anchor, CoreInput, ShapeCatalog, ShapeSpec};
