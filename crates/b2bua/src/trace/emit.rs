//! The guarded emission vocabulary of a traced call (ADR-0026).
//!
//! Every function here is reached ONLY from inside an `if trace::sampled(&call)`
//! guard at its call site, so an unsampled call never evaluates an argument,
//! never serializes a message and never allocates. Each one re-checks the flag
//! as a belt: a helper that escaped its guard must still cost nothing.
//!
//! Facts carry the timestamp of the fact, not of the emission — a backfilled
//! activation records the INVITE and the decision round trip at the times they
//! actually happened.

use std::net::SocketAddr;

use call::{Call, TimerEntry, TimerKey};
use observe::TraceEvent;

use super::registry::{traces, RoundTripSide};

pub use crate::turn_notes::sampled;

/// A SIP message received for the call, with its raw wire bytes.
pub fn sip_in(call: &Call, at_ms: i64, src: SocketAddr, wire: &[u8]) {
    if !sampled(call) {
        return;
    }
    record(call, "sip.in", at_ms, &format!("from {src}"), wire);
}

/// A SIP message sent for the call, with its raw wire bytes.
pub fn sip_out(call: &Call, at_ms: i64, dest: SocketAddr, wire: &[u8]) {
    if !sampled(call) {
        return;
    }
    record(call, "sip.out", at_ms, &format!("to {dest}"), wire);
}

/// A SIP message the stack sent on the call's behalf whose bytes this layer
/// never held — the transaction layer's auto-100. Recorded so the backfilled
/// story of a call is not missing its first response.
pub fn sip_out_note(call: &Call, at_ms: i64, detail: &str) {
    if !sampled(call) {
        return;
    }
    record(call, "sip.out", at_ms, detail, b"");
}

/// One fact a turn's notes carried (ADR-0052 §4), on its call's trace.
pub fn turn_fact(call_ref: &str, fact: &crate::turn_notes::TraceFact) {
    traces().record(call_ref, TraceEvent::new(fact.kind, fact.at_ms, &fact.detail));
}

/// A limiter admit / refresh / release outcome.
pub fn limiter(call: &Call, at_ms: i64, action: &'static str, detail: &str) {
    if !sampled(call) {
        return;
    }
    record(call, "limiter", at_ms, &format!("{action}: {detail}"), b"");
}

/// A timer armed: its key, the generation it was armed under, its deadline.
pub fn timer_arm(call: &Call, at_ms: i64, entry: &TimerEntry) {
    if !sampled(call) {
        return;
    }
    let detail = format!("{:?} gen {} at {}", entry.key, entry.generation, entry.fire_at);
    record(call, "timer.arm", at_ms, &detail, b"");
}

/// A timer disarmed before its fire.
pub fn timer_cancel(call: &Call, at_ms: i64, key: &TimerKey) {
    if !sampled(call) {
        return;
    }
    record(call, "timer.cancel", at_ms, &format!("{key:?}"), b"");
}

/// One outbound HTTP round trip as a child span carrying both bodies.
#[allow(clippy::too_many_arguments)]
pub fn round_trip(
    call: &Call,
    route: &'static str,
    sent_at_ms: i64,
    request: &[u8],
    received_at_ms: i64,
    outcome: &str,
    response: &[u8],
) {
    if !sampled(call) {
        return;
    }
    traces().round_trip(
        &call.call_ref,
        route,
        RoundTripSide { at_ms: sent_at_ms, detail: route, body: request },
        RoundTripSide { at_ms: received_at_ms, detail: outcome, body: response },
    );
}

/// A traced call's handle, cheap to move into a detached task. Obtained ONLY
/// for a sampled call ([`TraceHandle::of`] is the guard), so a detached callout
/// on an unsampled call never serializes a payload.
#[derive(Debug, Clone)]
pub struct TraceHandle {
    call_ref: String,
}

impl TraceHandle {
    /// A handle for a traced call, `None` for an unsampled one.
    pub fn of(call: &Call) -> Option<Self> {
        sampled(call).then(|| Self { call_ref: call.call_ref.clone() })
    }

    /// One detached HTTP round trip as a child span carrying both bodies.
    pub fn round_trip(
        &self,
        route: &'static str,
        sent_at_ms: i64,
        request: &[u8],
        received_at_ms: i64,
        outcome: &str,
        response: &[u8],
    ) {
        traces().round_trip(
            &self.call_ref,
            route,
            RoundTripSide { at_ms: sent_at_ms, detail: route, body: request },
            RoundTripSide { at_ms: received_at_ms, detail: outcome, body: response },
        );
    }
}

/// Emit one fact. Every public helper above has already run the guard; this
/// runs it once more so a helper that ever escapes its guard still costs one
/// `Option<bool>` read.
fn record(call: &Call, kind: &'static str, at_ms: i64, detail: &str, body: &[u8]) {
    if !sampled(call) {
        return;
    }
    traces().record(&call.call_ref, TraceEvent::new(kind, at_ms, detail).with_body(body));
}
