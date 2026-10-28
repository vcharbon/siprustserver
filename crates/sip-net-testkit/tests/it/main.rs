//! Every `sip-net-testkit` integration test, in one binary (ADR-0030): a test
//! file is a module here, and a file left directly in `tests/` is a second copy
//! of the dependency graph. Only an ADR-0030 X2 exception stays there.
//!
//! Select one (the workspace build `just test` made, no rebuild):
//! `cargo nextest run --workspace -E 'package(=sip-net-testkit) & binary(it)' <module>::`.

mod contracts;
mod cross_rules;
mod simulated;
