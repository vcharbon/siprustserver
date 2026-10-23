//! The incarnation gate, ahead of every rule an event reaches.

use super::resolve::Resolution;
use super::RouterCtx;
use b2bua_sdk::event::CallEvent;

/// The incarnation gate: an event stamped with another incarnation than the
/// call live on its `call_ref` (`differs`) is a straggler of an earlier call
/// there — a timer fire or transaction timeout queued before that call's
/// release, a callout result, a late message on its Via or Contact — and is
/// dropped before any rule reads it; the caller answers a request as one
/// naming no call (481, RFC 3261 §12.2.2). An admit it carries releases its
/// key when owed, as one reaching no call does. An unstamped event passes.
pub(super) fn of_another_incarnation(
    ctx: &RouterCtx,
    event: &CallEvent,
    res: &Resolution,
    differs: impl FnOnce(&str) -> bool,
) -> bool {
    let Some(stamp) = res.incarnation.as_deref() else { return false };
    if !differs(stamp) {
        return false;
    }
    ctx.metrics.bump_other_incarnation_dropped();
    super::callouts::release_admit_fold_call(&ctx.limiter, event);
    tracing::debug!(
        call_ref = res.call_ref.as_deref().unwrap_or_default(),
        incarnation = stamp,
        kind = event.kind(),
        "event of another call incarnation dropped"
    );
    true
}
