//! Every `b2bua-runner-kit` integration test, in one binary (ADR-0030): a test file
//! is a module here, and a file left directly in `tests/` is a second copy of
//! the dependency graph. Only an ADR-0030 X2 exception stays there.
//!
//! Select one (the workspace build `just test` made, no rebuild):
//! `cargo nextest run --workspace -E 'package(=b2bua-runner-kit) & binary(it)' <module>::`.

// Shared fixtures, reached from a test module as `crate::support::…`.
mod support;

mod cdr_rabbitmq_delivery;
mod cdr_rabbitmq_real_broker;
