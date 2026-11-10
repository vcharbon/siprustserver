//! The call a stuck poll holds (ADR-0057 §6): its turn never returns, so the
//! exit discharges the record as the store holds it. The input the record
//! carries is then never run again, by the backup's takeover or by this
//! node's reclaim after its restart.

use std::sync::Arc;

use call::CallModelState;

use super::departure::primary_departed;
use super::release::{release_call, ReleaseKind};
use super::{off_wire, RouterCtx};

/// Discharge `call_ref` at `now_ms`, its parties left connected: no rule
/// runs and nothing is sent to them. A live record is forced terminal
/// through the one enforcement path: its CDR, the release of its limiter key
/// (owed whatever the record says: the stuck turn may have sent an admit),
/// the delete, the backup's copy included. A record the stuck turn already
/// landed terminated wrote its CDR then: its key is released and the call
/// deleted as it stands, a takeover copy whose primary lives released here
/// only. `true` when the call was resident and no longer is. The per-call
/// lock stays with the stuck turn, whose body the caller fenced off first.
pub(crate) async fn discharge_stuck(ctx: &Arc<RouterCtx>, call_ref: &str, now_ms: i64) -> bool {
    let Some(mut call) = ctx.state.peek(call_ref) else {
        return false;
    };
    tracing::error!(%call_ref, "the call a stuck poll holds: discharged");
    if call.state == CallModelState::Terminated {
        if ctx.state.is_takeover(call_ref) && !primary_departed(ctx, call_ref) {
            release_call(ctx, call_ref, ReleaseKind::SelfRelease { ended: true }).await;
        } else {
            ctx.limiter.release(call.limiter.key());
            ctx.state.log_delete(call_ref);
            release_call(ctx, call_ref, ReleaseKind::Terminated).await;
        }
    } else {
        call.limiter.owe_release();
        let label = &"the discharge of a call a stuck poll holds";
        let ending = off_wire::Ending::Forced { before: &call };
        off_wire::end(ctx, call_ref, ending, off_wire::Caller::Leave, now_ms, label).await;
    }
    !ctx.state.contains(call_ref)
}
