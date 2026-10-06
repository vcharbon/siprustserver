//! Every `e2e-model` integration test, in one binary (ADR-0030): a test file
//! is a module here, and a file left directly in `tests/` is a second copy of
//! the dependency graph. Only an ADR-0030 X2 exception stays there.
//!
//! Select one (the workspace build `just test` made, no rebuild):
//! `cargo nextest run --workspace -E 'package(=e2e-model) & binary(it)' <module>::`.

mod case_checks_model;
mod load_profile;
mod pooled_case;
