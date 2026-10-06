//! Every `call-limiter` integration test, in one binary (ADR-0030): a test file
//! is a module here, and a file left directly in `tests/` is a second copy of
//! the dependency graph. Only an ADR-0030 X2 exception stays there.
//!
//! Select one (the workspace build `just test` made, no rebuild):
//! `cargo nextest run --workspace -E 'package(=call-limiter) & binary(it)' <module>::`.

mod admit_carries_held;
mod keyed_by_call;
mod oracle;
