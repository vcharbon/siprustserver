//! The absolute instants a [`Call`] record holds, moved together from the clock
//! that wrote them into another node's clock frame.

use crate::model::{Call, Leg};

/// Re-anchor **deadband** (ms): an offset between two clock frames whose
/// magnitude is below this is replication transit latency and clock jitter,
/// not a clock disagreement worth correcting, and a re-anchor applies none of
/// it. Under a single-clock harness `receiver_now − origin_now` is pure
/// latency, so the re-anchor is a no-op there and a keepalive keeps the
/// deadline a strict SIP-transparency oracle expects. A real host clock step
/// is interval-sized (the keepalive cadence, hundreds of seconds) and clears
/// it by orders of magnitude; a sub-second offset does not meaningfully move
/// a 300 s keepalive or a 150 s setup timer. One event read through two
/// re-anchored frames reads at most twice this apart.
pub const REANCHOR_DEADBAND_MS: i64 = 1_000;

/// The part of `skew_offset_ms`, a body's offset from this node's clock, a
/// re-anchor applies: all of it once it clears [`REANCHOR_DEADBAND_MS`], none
/// below.
pub fn reanchor_offset(skew_offset_ms: i64) -> i64 {
    if skew_offset_ms.abs() < REANCHOR_DEADBAND_MS {
        0
    } else {
        skew_offset_ms
    }
}

/// Move every absolute instant `call` holds by `offset_ms` (the reading
/// clock minus the writing clock): the timer ledger's deadlines, the creation
/// time, each dialled INVITE's send time, each NOTIFY in flight's send time, each
/// request record's deadline, the CDR events, the message rings, the decision log, the termination record and the
/// start of a transfer or reroute. Durations and counters are left as they
/// are, and so is the opaque service `ext`. A zero offset changes nothing.
pub fn shift_instants(call: &mut Call, offset_ms: i64) {
    if offset_ms == 0 {
        return;
    }
    let shift = |at: &mut i64| *at = at.saturating_add(offset_ms);
    for timer in &mut call.timers {
        shift(&mut timer.fire_at);
    }
    shift(&mut call.created_at);
    for event in &mut call.cdr_events {
        shift(&mut event.timestamp);
    }
    for mark in &mut call.decision_log {
        shift(&mut mark.at_ms);
    }
    if let Some(termination) = call.termination.as_mut() {
        shift(&mut termination.at_ms);
    }
    if let Some(transfer) = call.transfer.as_mut() {
        shift(&mut transfer.started_at_ms);
    }
    if let Some(reroute) = call.reroute.as_mut() {
        shift(&mut reroute.started_at_ms);
    }
    shift_leg(&mut call.a_leg, offset_ms);
    for leg in &mut call.b_legs {
        shift_leg(leg, offset_ms);
    }
}

/// The instants one leg holds: its INVITE's send time, its message ring, each
/// dialog's NOTIFYs in flight and the deadline of each request record.
fn shift_leg(leg: &mut Leg, offset_ms: i64) {
    if let Some(invite) = leg.outbound_invite_mut() {
        invite.sent_at_ms = invite.sent_at_ms.saturating_add(offset_ms);
    }
    for entry in &mut leg.messages.entries {
        entry.at_ms = entry.at_ms.saturating_add(offset_ms);
    }
    for dialog in leg.dialogs.iter_mut() {
        for notify in dialog.subscriptions.iter_mut().filter_map(|s| s.notify.as_mut()) {
            notify.sent_at_ms = notify.sent_at_ms.saturating_add(offset_ms);
        }
        for record in dialog.requests.iter_mut() {
            shift_deadline(record, offset_ms);
        }
        if let Some(retired) = dialog.retired_mut() {
            for record in retired.open_requests.iter_mut() {
                shift_deadline(record, offset_ms);
            }
        }
    }
}

fn shift_deadline(record: &mut crate::model::RequestRecord, offset_ms: i64) {
    if let Some(at) = record.deadline_ms.as_mut() {
        *at = at.saturating_add(offset_ms);
    }
}
