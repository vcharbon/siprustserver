//! The **axis data model** shared by both e2e run surfaces (ADR-0018/0019 and
//! the loadgen fusion): one dependency-light crate holding the authored JSON
//! documents (Test case / Check set / Campaign / Endpoint config) with their
//! loaders and load-time compatibility validation, the canonical Message-anchor
//! vocabulary, the layout-owned egress-rewrite model, and the post-call check
//! evaluator.
//!
//! Deliberately light: NO SUT crates (b2bua, sip-proxy, call, sip-txn, media)
//! — only serde/schemars/regex plus the message/recording surface the
//! evaluator reads (sip-message, sip-net, scenario-harness). The heavy run
//! machinery (`CallflowShape::run`, `InfraShape`/`InfraRuntime`, the cell
//! executor) stays in `e2e-core`, which re-exports everything here so its
//! consumers (e2e-cli, e2e-web, xtask) are unchanged. The seam between the
//! two is [`shape::ShapeSpec`]: the load-time metadata slice of a Callflow
//! shape that `validate_case` consumes without knowing how the shape runs.

pub mod checks;
pub mod egress;
pub mod endpoint;
pub mod model;
pub mod shape;

pub use checks::{Bindings, CheckVerdict, all_passed, evaluate_blocks, evaluate_case};
pub use egress::{
    ApiCall, ApiCallDestination, ApiCallRoute, CalleeTarget, EgressPolicy, EgressRewrite,
};
pub use endpoint::EndpointConfig;
pub use model::{
    Campaign, Check, CheckBlock, CheckOp, CheckSet, Concurrency, Input, ModelError, TestCase,
    load_campaign, load_check_set, load_check_sets, load_endpoint_config, load_test_case,
    schemas, validate_case,
};
pub use shape::{Anchor, CoreInput, ShapeSpec};
