//! An unrecoverable call (ADR-0052 §11): one whose record does not rebuild its
//! caller's INVITE, or holds a stored image a seed of its copy needs that does
//! not read. It is discharged silently, as the reaper's second strike discharges
//! a call (ADR-0020 X6): its legs are abandoned, its CDR and its limiter release
//! go through the one enforcement path with nothing on the wire, the call is
//! removed, and the discharge is counted. The request that met it is answered
//! as one for a vanished call.

use std::sync::Arc;

use b2bua_sdk::event::CallEvent;
use call::Call;
use sip_ids::TurnIds;

use super::off_wire;
use super::unroutable::answer_vanished;
use super::RouterCtx;
use crate::store::{Enveloped, MaterialiseOrigin};

/// Whether the materialised `copy` rebuilds its caller's INVITE and every image
/// its seeds name at `now_ms` reads back. One that fails either cannot serve the
/// transactions its record names, and the record may be behind the wire, so it
/// sends nothing: it is made resident under `origin` and discharged. An image no
/// seed names costs a live call nothing. The caller holds the per-call lock.
pub(super) async fn copy_recoverable(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    copy: &Enveloped,
    origin: MaterialiseOrigin,
    now_ms: i64,
) -> bool {
    let Err(err) = record_reads(&copy.call, now_ms) else {
        return true;
    };
    if ctx.state.materialize_if_absent(copy.clone(), origin) {
        discharge(ctx, call_ref, copy.call.clone(), now_ms, &err).await;
    }
    false
}

/// Whether `call`'s record serves a copy of it at `now_ms`: `Err` names the part
/// that does not read.
pub(super) fn record_reads(call: &Call, now_ms: i64) -> Result<(), String> {
    if let Err(err) = crate::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite) {
        return Err(format!("the caller's INVITE does not rebuild: {err}"));
    }
    match super::materialise::unreadable_seed_image(call, now_ms) {
        Some(refusal) => Err(format!("an image a seed names does not read: {refusal}")),
        None => Ok(()),
    }
}

/// The call whose turn on `event` met a record that does not rebuild its
/// caller's INVITE ([`crate::turn_notes::TurnNotes::unrecoverable`]): the
/// turn's result is dropped, the resident call is discharged, and the request
/// `event` carries is answered as one for a vanished call, its To-tag drawn
/// from `ids`. Returns whether a resident call was discharged. The caller
/// holds the per-call lock.
pub(super) async fn discharge_after_turn(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    event: &CallEvent,
    now_ms: i64,
    ids: &mut TurnIds,
) -> bool {
    let resident = ctx.state.peek(call_ref);
    let discharged = resident.is_some();
    if let Some(call) = resident {
        discharge(ctx, call_ref, call, now_ms, &"a step of its turn read it").await;
    }
    answer_vanished(ctx, event, ids).await;
    discharged
}

/// Discharge the resident `call` with nothing on the wire.
async fn discharge(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    call: Call,
    now_ms: i64,
    err: &impl std::fmt::Display,
) {
    ctx.metrics.bump_unrecoverable_call();
    tracing::error!(%call_ref, error = %err, "the call's record does not serve a copy: discharged");
    let ending = off_wire::Ending::Forced { before: &call };
    let label = &"the discharge of an unrecoverable call";
    off_wire::end(ctx, call_ref, ending, off_wire::Caller::Leave, now_ms, label).await;
}
