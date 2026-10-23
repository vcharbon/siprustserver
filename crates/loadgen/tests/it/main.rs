//! Every `loadgen` integration test, in one binary (ADR-0030): a test file
//! is a module here, and a file left directly in `tests/` is a second copy of
//! the dependency graph. Only an ADR-0030 X2 exception stays there.
//!
//! Every `loadgen` test, here and in the lib, is `#[ignore = "slow lane: loadgen"]`
//! (CLAUDE.md test-runtime policy).
//!
//! Select one (the workspace build `just test` made, no rebuild):
//! `cargo nextest run --workspace -E 'package(=loadgen) & binary(it)' --run-ignored only <module>::`.

mod drain;
mod fake_net;
mod fake_rig;
mod from_user_net;
mod governor;
mod late_final;
mod mux_claims;
mod mux_from_user;
mod outcome_records;
mod smoke;
mod waiver_lane;
