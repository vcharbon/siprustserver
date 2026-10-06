//! Every `e2e-core` integration test, in one binary (ADR-0030): a test file
//! is a module here, and a file left directly in `tests/` is a second copy of
//! the dependency graph. Only an ADR-0030 X2 exception stays there.
//!
//! Select one (the workspace build `just test` made, no rebuild):
//! `cargo nextest run --workspace -E 'package(=e2e-core) & binary(it)' <module>::`.

mod checks;
mod job;
mod model;
mod portability;
mod reaped;
mod result;
mod shapes_media;
