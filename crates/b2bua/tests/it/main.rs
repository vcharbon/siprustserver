//! Every `b2bua` integration test, in one binary (ADR-0030): a test file
//! is a module here, and a file left directly in `tests/` is a second copy of
//! the dependency graph. Only an ADR-0030 X2 exception stays there.
//!
//! Select one (the workspace build `just test` made, no rebuild):
//! `cargo nextest run --workspace -E 'package(=b2bua) & binary(it)' <module>::`.

mod ingress_brake;
mod materialised_admit_numbers;
mod media_leg_advertisement;
mod reaper_ledger;
mod rules;
mod service_macro;
mod udp_transport_metrics;
