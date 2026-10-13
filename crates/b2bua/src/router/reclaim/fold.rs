//! How a fold lands (ADR-0014, ADR-0031 D3): the other owner's body becomes
//! the live copy at the `max` of both views' counters, with no write of this
//! node's own, and the live timer service follows its ledger.

use std::sync::Arc;

use call::Call;

use super::super::restore_hygiene::sanitize_restored_timers;
use super::super::RouterCtx;

/// Take the other owner's body as the live copy. A fold is not this node's
/// write: the body takes `max` of the two views on each axis
/// ([`adopt_counters`]) and replaces the live copy without a bump of this
/// node's own axis ([`CallState::update_quiet`](crate::store::CallState::update_quiet))
/// and without a flush, so the next write of either owner orders against the
/// version the other owner published (ADR-0014, "one increment means"). The
/// live timer service follows the folded ledger ([`resync_timers`]).
pub(super) async fn fold_quietly(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    live: &Call,
    mut folded: Call,
    skew_offset_ms: i64,
    now_ms: i64,
) {
    adopt_counters(&mut folded, live);
    resync_timers(ctx, call_ref, live, &mut folded, skew_offset_ms, now_ms).await;
    ctx.state.update_quiet(folded);
}

/// A fold adopts BOTH counters: the folded body takes `max` of the two views on
/// each axis. Every version either owner published is then at or below the
/// live copy's, so this node's next write dominates them and the split vector
/// heals instead of each side refusing the other for ever (ADR-0031 D3). A body
/// without topology is not replicable and is left alone.
fn adopt_counters(folded: &mut Call, live: &Call) {
    if let (Some(f), Some(l)) = (folded.topology.as_mut(), live.topology.as_ref()) {
        f.gen = f.gen.max(l.gen);
        f.bak_gen = f.bak_gen.max(l.bak_gen);
    }
}

/// The live timer service follows the folded ledger: every entry the folded
/// body no longer carries is cancelled, every entry it carries is (re)armed in
/// this node's clock frame through the restore-hygiene seam, and the folded
/// body keeps the re-anchored ledger it was armed from.
async fn resync_timers(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    live: &Call,
    folded: &mut Call,
    skew_offset_ms: i64,
    now_ms: i64,
) {
    for stale in live.timers.iter().filter(|t| !folded.timers.iter().any(|f| f.key == t.key)) {
        ctx.timers.cancel(call_ref.to_string(), stale.key.clone()).await;
    }
    sanitize_restored_timers(
        &mut folded.timers,
        call_ref,
        now_ms,
        Some(skew_offset_ms),
        ctx.config.keepalive_interval_sec * 1000,
        None,
    );
    ctx.timers.restore(folded.timers.clone(), call_ref.to_string(), folded.incarnation()).await;
}
