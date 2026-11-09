//! The call a stuck poll holds (ADR-0057 §6): its turn never returns, so the
//! exit discharges the record as the store holds it, with nothing on the
//! wire. The input the record carries is then never run again, by the
//! backup's takeover or by this node's reclaim after its restart.

use std::sync::Arc;

use call::CallModelState;

use super::{off_wire, RouterCtx};

/// Discharge `call_ref` off the wire at `now_ms`, its parties left
/// connected: the CDR, the limiter release and the delete, the backup's copy
/// included, go through the one enforcement path. A record the stuck turn
/// already landed terminated is ended as it stands. `true` when the call was
/// resident and no longer is. The per-call lock stays with the stuck turn,
/// whose body the caller fenced off first.
pub(crate) async fn discharge_stuck(ctx: &Arc<RouterCtx>, call_ref: &str, now_ms: i64) -> bool {
    let Some(call) = ctx.state.peek(call_ref) else {
        return false;
    };
    tracing::error!(%call_ref, "the call a stuck poll holds: discharged off the wire");
    let label = &"the discharge of a call a stuck poll holds";
    let leave = off_wire::Caller::Leave;
    if call.state == CallModelState::Terminated {
        let ending = off_wire::Ending::Reached { body: call, forced: false };
        off_wire::end(ctx, call_ref, ending, leave, now_ms, label).await;
    } else {
        let ending = off_wire::Ending::Forced { before: &call };
        off_wire::end(ctx, call_ref, ending, leave, now_ms, label).await;
    }
    !ctx.state.contains(call_ref)
}
