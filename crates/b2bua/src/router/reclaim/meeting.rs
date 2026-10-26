//! Where a copy of the call the other owner wrote meets the live copy here
//! (ADR-0055 §7). The copy that contains the other is taken whole, the
//! other's exempt fields merged in; two copies that diverged end the call
//! cleanly ([`super::diverged`]). A meeting is not this node's write: the
//! live copy is replaced without a bump of this node's axis
//! ([`CallState::update_quiet`](crate::store::CallState::update_quiet)) and
//! without a flush.

use std::sync::Arc;

use call::helpers::lifecycle_behind;
use call::{Call, CallModelState};

use super::super::materialise::answered_invite_seeds;
use super::super::release::{release_call, ReleaseKind};
use super::super::restore_hygiene::sanitize_restored_timers;
use super::super::stand_down::covers;
use super::super::RouterCtx;
use super::diverged::{end_diverged, end_over_an_ending, own_records};
use super::ending::discharge_folded_terminal;
use super::{format_pb, FlushDirection};
use crate::store::containment::{meet, standing, Containment, Met};
use crate::store::Enveloped;

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
///   stands it down ([`covers`]). Otherwise the copy plays on: with the
///   primary's exempt fields merged in when each contains the other, or on
///   the primary's copy taken whole when only the primary's contains it, the
///   copy's own open request records carried into it — a vector is never
///   taken without the body it counts.
///
/// Copies that diverged end the call cleanly from here ([`end_diverged`]).
pub(in crate::router) async fn meet_live(
    ctx: &Arc<RouterCtx>,
    direction: FlushDirection,
    call_ref: &str,
    live: &Enveloped,
    incoming: Enveloped,
    now_ms: i64,
) {
    let incoming_is_primary = matches!(direction, FlushDirection::Forward);
    let met = meet(live.clone(), &incoming, incoming_is_primary);
    let label = match &met {
        Met::Held(_) => "held",
        Met::Incoming(_) => "incoming",
        Met::Diverged => "diverged",
    };
    tracing::debug!(
        node = observe::node(),
        call_ref,
        call_id = %live.call.a_leg.call_id,
        direction = direction.label(),
        met = label,
        state = ?incoming.call.state,
        replica_pb = %format_pb(incoming.topology.as_ref()),
        live_pb = %format_pb(live.topology.as_ref()),
        "copies met"
    );
    let this = &ctx.relay_owner.ordinal;
    let peer = live.topology.as_ref().and_then(|t| t.other_owner(this)).unwrap_or("unknown");
    ctx.meeting_waves.record(peer, label, 1);
    let ending = incoming.call.state != CallModelState::Active;
    if let (FlushDirection::Reverse, true) = (direction, ending) {
        let ends_here = match &met {
            Met::Incoming(_) => true,
            Met::Diverged => lifecycle_behind(&live.call, &incoming.call),
            Met::Held(_) => false,
        };
        if ends_here {
            let diverged =
                matches!(met, Met::Diverged) && live.call.state == CallModelState::Active;
            if diverged && end_over_an_ending(ctx, call_ref, live, &incoming, now_ms).await {
                return;
            }
            if incoming.call.state == CallModelState::Terminated {
                discharge_folded_terminal(ctx, call_ref, &live.call, incoming, now_ms).await;
            }
            return;
        }
    }
    let by = &ctx.relay_owner;
    let both = standing(live, &incoming) == Some(Containment::Both);
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
            FlushDirection::Forward if covers(&incoming, live, by) => {
                release_call(ctx, call_ref, ReleaseKind::StandDown).await;
            }
            FlushDirection::Forward if both => {
                keep(ctx, live, live.clone().merge_exempt(&incoming));
            }
            FlushDirection::Forward => {
                let Enveloped { call: taken, topology } = taken;
                let mut taken = own_records(taken, &live.call, by);
                resync_timers(ctx, call_ref, &live.call, &mut taken, now_ms).await;
                ctx.state.update_enveloped(Enveloped::new(taken, topology));
            }
        },
        Met::Diverged => {
            end_diverged(ctx, direction, call_ref, live, incoming, now_ms).await;
        }
    }
}

/// The live copy stands, `kept` being it with the other copy's exempt fields
/// merged in.
fn keep(ctx: &Arc<RouterCtx>, live: &Enveloped, kept: Enveloped) {
    if kept != *live {
        ctx.state.update_enveloped(kept);
    }
}

/// The record takes `taken`, the other owner's copy whole with the live
/// copy's exempt fields merged in. A copy that does not rebuild its caller's
/// INVITE never replaces a live one (ADR-0052 §11): it is counted, and the
/// live copy stands at its own vector — a vector is never taken without the
/// body it counts — with the other's exempt fields. The live timer service
/// follows the ledger taken, and every INVITE it holds answered 2xx and
/// awaiting its ACK is seeded in the layer, so a copy of that INVITE draws
/// the 2xx (an identity already held is left as it is).
async fn take_whole(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    live: &Enveloped,
    incoming: &Enveloped,
    taken: Enveloped,
    now_ms: i64,
) {
    if let Err(err) = crate::rules::relay::rebuild_a_leg_invite(&taken.call.a_leg_invite) {
        ctx.metrics.bump_unrecoverable_meeting();
        tracing::error!(%call_ref, error = %err, "a met copy does not rebuild the caller's INVITE: the live copy stays");
        let mut kept = live.clone().merge_exempt(incoming);
        kept.topology.clone_from(&live.topology);
        keep(ctx, live, kept);
        return;
    }
    let Enveloped { call: mut taken, topology } = taken;
    resync_timers(ctx, call_ref, &live.call, &mut taken, now_ms).await;
    let seeds = answered_invite_seeds(&taken);
    ctx.state.update_enveloped(Enveloped::new(taken, topology));
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
