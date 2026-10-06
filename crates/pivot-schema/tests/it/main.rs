//! Every `pivot-schema` integration test, in one binary (ADR-0030): a test file
//! is a module here, and a file left directly in `tests/` is a second copy of
//! the dependency graph. Only an ADR-0030 X2 exception stays there.
//!
//! Select one (the workspace build `just test` made, no rebuild):
//! `cargo nextest run --workspace -E 'package(=pivot-schema) & binary(it)' <module>::`.

mod bundle;
mod conformance;
mod lint;
mod part_compare;
mod recording_arms;
mod recording_fixture;
mod recording_wire;
