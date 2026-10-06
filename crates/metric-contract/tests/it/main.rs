//! Every `metric-contract` integration test, in one binary (ADR-0030): a test
//! file is a module here.
//!
//! Select one (the workspace build `just test` made, no rebuild):
//! `cargo nextest run --workspace -E 'package(=metric-contract) & binary(it)' <module>::`.

mod deploy_readers;
mod export;
