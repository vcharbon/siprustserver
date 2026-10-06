//! Every `scenario-harness` integration test, in one binary (ADR-0030): a test file
//! is a module here, and a file left directly in `tests/` is a second copy of
//! the dependency graph. Only an ADR-0030 X2 exception stays there.
//!
//! Select one (the workspace build `just test` made, no rebuild):
//! `cargo nextest run --workspace -E 'package(=scenario-harness) & binary(it)' <module>::`.

mod ack_obligation;
mod actor_plan_deviations;
mod advance_settles;
mod alice_calls_bob;
mod any_method_prack_update;
mod callee_group;
mod deviations;
mod early_dialogs;
mod finish_settles;
mod fluent_dialog;
mod harness_log_capture;
mod http_exchanges;
mod proxy_record_route;
mod shared_endpoint;
mod snapshot_report;
mod template_emission;
mod timed_dialog;
mod waivers;
