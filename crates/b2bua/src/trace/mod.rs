//! Per-call tracing for the B2BUA (ADR-0026).
//!
//! One concern per submodule: [`registry`] owns the process's admission gate
//! and the live root spans; [`emit`] owns the guarded emission vocabulary — the
//! SIP messages, rule transitions, limiter and HTTP round trips a traced call
//! records.
//!
//! **Explicit-guard discipline.** Every emission site is wrapped in
//! `if trace::sampled(&call) { … }`. An unsampled call constructs no span,
//! serializes no message and allocates nothing extra; the guard is a single
//! `Option<bool>` read on the call it already holds.

pub mod emit;
pub mod registry;

pub use emit::sampled;
pub use registry::{adopt_into, adopt_replicated, install_process_traces, traces, CallTraces};
