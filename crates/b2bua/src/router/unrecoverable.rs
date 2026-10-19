//! An unrecoverable call (ADR-0052 §11): one whose record does not rebuild
//! its caller's INVITE, which every turn takes as an input. It is discharged
//! silently, as the reaper's second strike discharges a call (ADR-0020 X6):
//! its legs are abandoned, its CDR and its limiter release go through the one
//! enforcement path with nothing on the wire, the call is removed, and the
//! discharge is counted.

use std::sync::Arc;

use call::Call;
use sip_message::SipRequest;

use super::interpret::process_result;
use super::RouterCtx;
use crate::rules::invariants::{enforce, finalize, UnansweredCaller};
use crate::store::MaterialiseOrigin;

/// The caller's INVITE of the resident `call`, rebuilt for its turn; `None`
/// once a call whose record does not rebuild it is discharged. The caller
/// holds the per-call lock.
pub(super) async fn caller_or_discharge(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    call: &Call,
    now_ms: i64,
) -> Option<SipRequest> {
    match crate::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite) {
        Ok(caller) => Some(caller),
        Err(err) => {
            discharge(ctx, call_ref, call.clone(), now_ms, &err).await;
            None
        }
    }
}

/// Whether the materialised `copy` rebuilds its caller's INVITE; one that does
/// not is made resident under `origin` and discharged. The caller holds the
/// per-call lock.
pub(super) async fn copy_recoverable(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    copy: &Call,
    origin: MaterialiseOrigin,
    now_ms: i64,
) -> bool {
    let Err(err) = crate::rules::relay::rebuild_a_leg_invite(&copy.a_leg_invite) else {
        return true;
    };
    if ctx.state.materialize_if_absent(copy.clone(), origin) {
        discharge(ctx, call_ref, copy.clone(), now_ms, &err).await;
    }
    false
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
    let result = enforce(&ctx.obligations, &before, discharged, now_ms, UnansweredCaller::Leave);
    process_result(ctx, call_ref, result, now_ms, &"the discharge of an unrecoverable call", None)
        .await;
}
