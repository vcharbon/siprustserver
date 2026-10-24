//! An unrecoverable call (ADR-0052 §11): one whose record does not rebuild
//! its caller's INVITE. It is discharged silently, as the reaper's second
//! strike discharges a call (ADR-0020 X6): its legs are abandoned, its CDR and
//! its limiter release go through the one enforcement path with nothing on the
//! wire, the call is removed, and the discharge is counted. The request that
//! met it is answered as one for a vanished call.

use std::sync::Arc;

use b2bua_sdk::event::CallEvent;
use call::Call;
use sip_ids::TurnIds;

use super::interpret::process_result;
use super::unroutable::answer_vanished;
use super::RouterCtx;
use crate::rules::invariants::{enforce, finalize, UnansweredCaller};
use crate::store::{Enveloped, MaterialiseOrigin};

/// Whether the materialised `copy` rebuilds its caller's INVITE; one that does
/// not is made resident under `origin` and discharged. The caller holds the
/// per-call lock.
pub(super) async fn copy_recoverable(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    copy: &Enveloped,
    origin: MaterialiseOrigin,
    now_ms: i64,
) -> bool {
    let Err(err) = crate::rules::relay::rebuild_a_leg_invite(&copy.call.a_leg_invite) else {
        return true;
    };
    if ctx.state.materialize_if_absent(copy.clone(), origin) {
        discharge(ctx, call_ref, copy.call.clone(), now_ms, &err).await;
    }
    false
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
    tracing::error!(%call_ref, error = %err, "the call's record does not rebuild its caller's INVITE: discharged");
    let before = call.clone();
    let discharged = finalize(crate::reaper::discharge_result(call, now_ms));
    let mut notes = crate::turn_notes::TurnNotes::default();
    let leave = UnansweredCaller::Leave;
    let result = enforce(&ctx.obligations, &before, discharged, now_ms, leave, &mut notes);
    super::notes::drain_notes(ctx, call_ref, notes);
    process_result(ctx, call_ref, result, now_ms, &"the discharge of an unrecoverable call", None)
        .await;
}
