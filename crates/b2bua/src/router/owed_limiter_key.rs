//! The limiter release an ended body still owes, sent with no turn: no CDR,
//! no delete. A never-reclaimed deferred terminal at the replica reap, and an
//! ended call of a primary read dead at its verdict
//! ([`super::departure`]), are released through it.

use call::Call;

use super::RouterCtx;

/// Release the cluster-wide limiter set `call` still owns
/// (`CallLimiterState::owed_release`: a call that sent no admit owes nothing);
/// whether a release left. The release is keyed by the call, so one another
/// node sent too frees nothing more.
pub(super) fn release(ctx: &RouterCtx, call: &Call) -> bool {
    let Some(key) = call.limiter.owed_release() else { return false };
    ctx.limiter.release(key);
    true
}
