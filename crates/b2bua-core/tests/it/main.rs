//! Every `b2bua-core` integration test, in one binary (ADR-0030): the
//! synchronous rule tests, which run a turn's core with no runtime.
//!
//! Select one (the workspace build `just test` made, no rebuild):
//! `cargo nextest run --workspace -E 'package(=b2bua-core) & binary(it)' <module>::`.

mod caller_tags;
mod calls;
mod ended_dialog_2xx_rules;
mod evidence_rules;
mod evidence_triggers;
mod fork_straggler_rules;
mod lifecycle_transition_trace;
mod media_leg_advertisement;
mod rules;
mod service_macro;
mod services;
mod timer_scope_rules;
mod turn_lane;
mod unheld_lane;
mod unheld_response;

/// The caller's INVITE `call`'s record rebuilds.
fn caller_of(call: &call::Call) -> sip_message::SipRequest {
    b2bua_core::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite)
        .expect("the test call's record rebuilds its caller's INVITE")
}
