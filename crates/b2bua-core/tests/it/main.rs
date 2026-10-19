//! Every `b2bua-core` integration test, in one binary (ADR-0030): the
//! synchronous rule tests, which run a turn's core with no runtime.
//!
//! Select one (the workspace build `just test` made, no rebuild):
//! `cargo nextest run --workspace -E 'package(=b2bua-core) & binary(it)' <module>::`.

mod lifecycle_transition_trace;
mod lost_round_rules;
mod media_leg_advertisement;
mod rules;
mod service_macro;

/// The caller's INVITE `call`'s record rebuilds: what the shell hands each of
/// its turns (ADR-0052 §11).
fn caller_of(call: &call::Call) -> sip_message::SipRequest {
    b2bua_core::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite)
        .expect("the test call's record rebuilds its caller's INVITE")
}

/// [`caller_of`], leaked so a helper can hand a context holding it back.
fn leaked_caller_of(call: &call::Call) -> &'static sip_message::SipRequest {
    Box::leak(Box::new(caller_of(call)))
}
