//! Every `sip-proxy` integration test, in one binary (ADR-0030): a test file
//! is a module here, and a file left directly in `tests/` is a second copy of
//! the dependency graph. Only an ADR-0030 X2 exception stays there.
//!
//! Select one (the workspace build `just test` made, no rebuild):
//! `cargo nextest run --workspace -E 'package(=sip-proxy) & binary(it)' <module>::`.

// Shared fixtures, reached from a test module as `crate::common::…`.
mod common;

mod first_view;
mod health_probe_late_reply;
mod ingress_trust;
mod load_balancer;
mod load_balancer_routing;
mod no_panic_lane;
mod options_e2e;
mod per_call_trace;
mod probe_period;
mod refused_invite_copies;
mod rfc_proxy_compliance;
mod select_failure_503;
mod self_gate_admission;
mod shard_e2e;
mod stateless_final_response_contract;
mod transit_only;
