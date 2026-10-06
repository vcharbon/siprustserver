//! Every `loadgen` integration test, in one binary (ADR-0030): a test file
//! is a module here, and a file left directly in `tests/` is a second copy of
//! the dependency graph. Only an ADR-0030 X2 exception stays there.
//!
//! Every `loadgen` test, here and in the lib, is `#[ignore = "slow lane: loadgen"]`
//! (CLAUDE.md test-runtime policy).
//!
//! Select one (the workspace build `just test` made, no rebuild):
//! `cargo nextest run --workspace -E 'package(=loadgen) & binary(it)' --run-ignored only <module>::`.

mod fake_net;
mod from_user_net;
mod governor;
mod mux_claims;
mod mux_from_user;
mod smoke;
mod waiver_lane;
