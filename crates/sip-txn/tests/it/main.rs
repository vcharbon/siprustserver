//! Every `sip-txn` integration test, in one binary (ADR-0030): a test file
//! is a module here, and a file left directly in `tests/` is a second copy of
//! the dependency graph. Only an ADR-0030 X2 exception stays there.
//!
//! Select one (the workspace build `just test` made, no rebuild):
//! `cargo nextest run --workspace -E 'package(=sip-txn) & binary(it)' <module>::`.

// Shared fixtures, reached from a test module as `crate::common::…`.
mod common;

mod absorb_100;
mod bounded_queue;
mod branch_collision;
mod cancel_after_final;
mod cancel_hold;
mod cancel_ladder;
mod cancel_on_evict;
mod cancel_retransmit;
mod fsm;
mod handles;
mod invite_give_up;
mod late_cancel_hold;
mod lifetime;
mod long_ring_holds;
mod request_back_to_sender;
mod response_leaves_as_its_image;
mod seed;
mod shared_refusals;
mod to_tag_binding;
