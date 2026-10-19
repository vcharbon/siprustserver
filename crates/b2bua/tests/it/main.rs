//! Every `b2bua` integration test, in one binary (ADR-0030): a test file
//! is a module here, and a file left directly in `tests/` is a second copy of
//! the dependency graph. Only an ADR-0030 X2 exception stays there.
//!
//! Select one (the workspace build `just test` made, no rebuild):
//! `cargo nextest run --workspace -E 'package(=b2bua) & binary(it)' <module>::`.

mod ingress_brake;
mod lifecycle_transition_trace;
mod lost_round_rules;
mod materialised_admit_numbers;
mod media_leg_advertisement;
mod reaper_ledger;
mod rules;
mod service_macro;
mod udp_transport_metrics;

/// The caller's INVITE `call`'s record rebuilds: what the shell hands each of
/// its turns (ADR-0052 §11).
fn caller_of(call: &call::Call) -> sip_message::SipRequest {
    b2bua::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite)
        .expect("the test call's record rebuilds its caller's INVITE")
}

/// [`caller_of`], leaked so a helper can hand a context holding it back.
fn leaked_caller_of(call: &call::Call) -> &'static sip_message::SipRequest {
    Box::leak(Box::new(caller_of(call)))
}
