//! Where a copy of the call the other owner wrote meets the live copy here
//! (ADR-0055 §7). The copy that contains the other is taken whole, the
//! other's exempt fields merged in; two copies that diverged end the call
//! cleanly ([`super::diverged`]). A meeting is not this node's write: the
//! live copy is replaced without a bump of this node's axis
//! ([`CallState::update_quiet`](crate::store::CallState::update_quiet)) and
//! without a flush.

use std::sync::Arc;

use call::helpers::{lifecycle_behind, merge_exempt};
use call::{Call, CallModelState};

use super::super::materialise::answered_invite_seeds;
use super::super::release::{release_call, ReleaseKind};
use super::super::restore_hygiene::sanitize_restored_timers;
use super::super::stand_down::covers;
use super::super::RouterCtx;
use super::diverged::end_diverged;
use super::ending::discharge_folded_terminal;
use super::{format_pb, FlushDirection};
use crate::store::containment::{meet, Met};

/// The other owner's copy `incoming` of the call meets the `live` copy here,
/// one call incarnation, under the per-call lock; `direction` says whose
/// copy it is.
///
/// - A **reverse** copy reaches the call's record. An ending copy that
///   contains the record, or that diverged from it and ranks higher on the
///   lifecycle, ends the call: `Terminated`, it is discharged here
///   ([`discharge_folded_terminal`]); `Terminating`, its `Terminated` write is
///   awaited. Otherwise the copy that contains the other stands: the copy
///   taken whole is the call's new record, its answered INVITEs seeded in
///   the layer ([`answered_invite_seeds`]).
/// - A **forward** copy is the primary's, reaching a takeover copy. A primary
///   that contains the copy and holds every request record it keeps open
///   stands it down ([`covers`]); otherwise the copy plays on, the primary's
///   exempt fields merged in.
///
/// Copies that diverged end the call cleanly from here ([`end_diverged`]).
pub(in crate::router) async fn meet_live(
    ctx: &Arc<RouterCtx>,
    direction: FlushDirection,
    call_ref: &str,
    live: &Call,
    incoming: Call,
    now_ms: i64,
) {
    let incoming_is_primary = matches!(direction, FlushDirection::Forward);
    let met = meet(live.clone(), &incoming, incoming_is_primary);
    let label = match &met {
        Met::Held(_) => "held",
        Met::Incoming(_) => "incoming",
        Met::Diverged => "diverged",
    };
    tracing::info!(
        node = observe::node(),
        call_ref,
        call_id = %live.a_leg.call_id,
        direction = direction.label(),
        met = label,
        state = ?incoming.state,
        replica_pb = %format_pb(&incoming),
        live_pb = %format_pb(live),
        "copies met"
    );
    let ending = incoming.state != CallModelState::Active;
    if let (FlushDirection::Reverse, true) = (direction, ending) {
        let ends_here = match &met {
            Met::Incoming(_) => true,
            Met::Diverged => lifecycle_behind(live, &incoming),
            Met::Held(_) => false,
        };
        if ends_here {
            if incoming.state == CallModelState::Terminated {
                discharge_folded_terminal(ctx, call_ref, live, incoming, now_ms).await;
            }
            return;
        }
    }
    match met {
        Met::Held(kept) => {
            if let FlushDirection::Reverse = direction {
                ctx.metrics.bump_repl_reverse_flush_refused();
            }
            keep(ctx, live, kept);
        }
        Met::Incoming(taken) => match direction {
            FlushDirection::Reverse => {
                take_whole(ctx, call_ref, live, &incoming, taken, now_ms).await
            }
            FlushDirection::Forward => {
                if covers(&incoming, live, &ctx.config.relay_owner()) {
                    release_call(ctx, call_ref, ReleaseKind::StandDown).await;
                } else {
                    keep(ctx, live, merge_exempt(live.clone(), &incoming));
                }
            }
        },
        Met::Diverged => {
            ctx.metrics.bump_repl_copies_diverged();
            end_diverged(ctx, direction, call_ref, live, incoming, now_ms).await;
        }
    }
}

/// The live copy stands, `kept` being it with the other copy's exempt fields
/// merged in.
fn keep(ctx: &Arc<RouterCtx>, live: &Call, kept: Call) {
    if kept != *live {
        ctx.state.update_quiet(kept);
    }
}

/// The record takes `taken`, the other owner's copy whole with the live
/// copy's exempt fields merged in. A copy that does not rebuild its caller's
/// INVITE never replaces a live one (ADR-0052 §11): it is counted, and the
/// live copy stands with the other's exempt fields. The live timer service
/// follows the ledger taken, and every INVITE it holds answered 2xx and
/// awaiting its ACK is seeded in the layer, so a copy of that INVITE draws
/// the 2xx (an identity already held is left as it is).
async fn take_whole(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    live: &Call,
    incoming: &Call,
    mut taken: Call,
    now_ms: i64,
) {
    if let Err(err) = crate::rules::relay::rebuild_a_leg_invite(&taken.a_leg_invite) {
        ctx.metrics.bump_unrecoverable_fold();
        tracing::error!(%call_ref, error = %err, "a met copy does not rebuild the caller's INVITE: the live copy stays");
        keep(ctx, live, merge_exempt(live.clone(), incoming));
        return;
    }
    resync_timers(ctx, call_ref, live, &mut taken, now_ms).await;
    let seeds = answered_invite_seeds(&taken);
    ctx.state.update_quiet(taken);
    if !seeds.is_empty() {
        let _ = ctx.txn.seed(call_ref, seeds).await;
    }
}

/// The live timer service follows the ledger `taken` holds: every entry it
/// no longer carries is cancelled, and every entry it took from the other
/// owner — one the live copy does not hold as it is — is (re)armed in this
/// node's clock frame through the restore-hygiene seam; `taken` keeps the
/// re-anchored ledger it was armed from.
pub(super) async fn resync_timers(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    live: &Call,
    taken: &mut Call,
    now_ms: i64,
) {
    for stale in live.timers.iter().filter(|t| !taken.timers.iter().any(|f| f.key == t.key)) {
        ctx.timers.cancel(call_ref.to_string(), stale.key.clone()).await;
    }
    let (kept, mut fresh): (Vec<_>, Vec<_>) =
        std::mem::take(&mut taken.timers).into_iter().partition(|t| live.timers.contains(t));
    sanitize_restored_timers(
        &mut fresh,
        call_ref,
        now_ms,
        ctx.config.keepalive_interval_sec * 1000,
        None,
    );
    ctx.timers.restore(fresh.clone(), call_ref.to_string(), taken.incarnation()).await;
    taken.timers = kept.into_iter().chain(fresh).collect();
}
