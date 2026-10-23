//! How a fold lands (ADR-0014, ADR-0031 D3): the other owner's body, or only
//! the re-INVITE rounds it stands ahead on, becomes the live copy at the
//! `max` of both views' counters and dialog sequences, with no write of this
//! node's own, and the live timer service follows the timers it took.

use std::sync::Arc;

use call::helpers::{merge_rounds, raise_sequences, Progress};
use call::Call;

use super::super::materialise::answered_invite_seeds;
use super::super::restore_hygiene::sanitize_restored_timers;
use super::super::RouterCtx;
use super::FlushDirection;

/// How much of the other owner's body a fold takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::router) enum Fold {
    /// The whole body: the vector says it is newer, or it is the call further
    /// along on the answer or the ending axis.
    Whole,
    /// Only the re-INVITE rounds the body stands ahead on: the end of one this
    /// node holds unfinished, the opening of one it never reached
    /// ([`call::helpers::merge_rounds`]); this node's own writes stay.
    Rounds,
}

impl Fold {
    /// The fold the progress a refused body carries calls for.
    pub(in crate::router) fn of(progress: Progress) -> Fold {
        match progress {
            Progress::Lifecycle => Fold::Whole,
            Progress::Round => Fold::Rounds,
        }
    }
}

/// Fold the other owner's body into the live copy. A fold is not this node's
/// write: the folded body carries `max` of the two views on each axis
/// ([`adopt_counters`]) and replaces the live copy without a bump of this
/// node's own axis ([`CallState::update_quiet`](crate::store::CallState::update_quiet))
/// and without a flush; this node's store keeps the version it last flushed.
/// A whole fold holds the other owner's content; a rounds fold holds this
/// node's content with the rounds merged in. Either holds on every dialog the
/// higher of the two views' sequences ([`call::helpers::raise_sequences`]):
/// this node's next request continues from every one the other owner sent
/// (RFC 3261 §12.2.1.1). Folded into the call's record (`direction`), the body
/// is the primary's, and every INVITE it holds answered 2xx and awaiting its
/// ACK is seeded in the layer, so a copy of that INVITE draws the 2xx
/// ([`answered_invite_seeds`]; an identity already held is left as it is).
/// The live timer service follows the timers the fold took.
#[allow(clippy::too_many_arguments)]
pub(in crate::router) async fn fold_quietly(
    ctx: &Arc<RouterCtx>,
    direction: FlushDirection,
    call_ref: &str,
    live: &Call,
    replica: Call,
    fold: Fold,
    now_ms: i64,
) {
    // A live copy is never replaced by a body that does not rebuild its
    // caller's INVITE (ADR-0052 §11): the fold is refused and counted, and
    // the version it names is still seen. A rounds fold keeps the live
    // copy's snapshot.
    if fold == Fold::Whole {
        if let Err(err) = crate::rules::relay::rebuild_a_leg_invite(&replica.a_leg_invite) {
            ctx.metrics.bump_unrecoverable_fold();
            tracing::error!(%call_ref, error = %err, "a folded body does not rebuild the caller's INVITE: the live copy stays");
            super::adopt_seen_counter(ctx, direction, live, &replica);
            return;
        }
    }
    let mut folded = match fold {
        Fold::Whole => raise_sequences(replica, live),
        Fold::Rounds => {
            let mut merged = raise_sequences(merge_rounds(live.clone(), &replica), &replica);
            adopt_counters(&mut merged, &replica);
            merged
        }
    };
    adopt_counters(&mut folded, live);
    if let (true, Some(t)) = (direction.into_the_record(), folded.topology.as_mut()) {
        t.gen_adopted = false;
    }
    resync_timers(ctx, call_ref, live, &mut folded, now_ms).await;
    let seeds = if direction.into_the_record() { answered_invite_seeds(&folded) } else { vec![] };
    ctx.state.update_quiet(folded);
    if !seeds.is_empty() {
        let _ = ctx.txn.seed(call_ref, seeds).await;
    }
}

/// A fold adopts BOTH counters: the folded body takes `max` of the two views on
/// each axis. Every version either owner published is then at or below the
/// live copy's, so this node's next write dominates them and the split vector
/// heals instead of each side refusing the other for ever (ADR-0031 D3). A `p`
/// raised past the folded content's own names a primary version that content
/// does not hold ([`call::CallTopology::gen_adopted`]); the call's record
/// clears that mark, its body being the primary's. A body without topology is
/// not replicable and is left alone.
fn adopt_counters(folded: &mut Call, other: &Call) {
    if let (Some(f), Some(o)) = (folded.topology.as_mut(), other.topology.as_ref()) {
        f.gen_adopted |= o.gen > f.gen;
        f.gen = f.gen.max(o.gen);
        f.bak_gen = f.bak_gen.max(o.bak_gen);
    }
}

/// The live timer service follows the folded ledger: every entry the folded
/// body no longer carries is cancelled, and every entry it took from the other
/// owner — one the live copy does not hold as it is — is (re)armed in this
/// node's clock frame through the restore-hygiene seam; the folded body keeps
/// the re-anchored ledger it was armed from.
async fn resync_timers(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    live: &Call,
    folded: &mut Call,
    now_ms: i64,
) {
    for stale in live.timers.iter().filter(|t| !folded.timers.iter().any(|f| f.key == t.key)) {
        ctx.timers.cancel(call_ref.to_string(), stale.key.clone()).await;
    }
    let (kept, mut taken): (Vec<_>, Vec<_>) =
        std::mem::take(&mut folded.timers).into_iter().partition(|t| live.timers.contains(t));
    sanitize_restored_timers(
        &mut taken,
        call_ref,
        now_ms,
        ctx.config.keepalive_interval_sec * 1000,
        None,
    );
    ctx.timers.restore(taken.clone(), call_ref.to_string(), folded.incarnation()).await;
    folded.timers = kept.into_iter().chain(taken).collect();
}
