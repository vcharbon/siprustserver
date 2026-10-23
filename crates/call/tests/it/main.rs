//! Every `call` integration test, in one binary (ADR-0030): a test file
//! is a module here, and a file left directly in `tests/` is a second copy of
//! the dependency graph. Only an ADR-0030 X2 exception stays there.
//!
//! Select one (the workspace build `just test` made, no rebuild):
//! `cargo nextest run --workspace -E 'package(=call) & binary(it)' <module>::`.

// Shared fixtures, reached from a test module as `crate::common::…`.
mod common;

mod attempts;
mod callref;
mod codec_roundtrip;
mod decode_bounds;
mod index_key;
mod instants;
mod invite_glare;
mod lifecycle;
mod model_helpers;
mod request_records;
mod round;
mod sequence;
mod timer_scope;
