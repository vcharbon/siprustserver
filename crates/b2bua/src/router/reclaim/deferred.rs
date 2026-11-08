//! The bulk reclaim's steps deferred to a turn of the call's own queue when a
//! turn holds the call's lock, so the pass never waits on one: the park, and
//! the arming of a deferred park. Each runs under the call's lock as the pass
//! would have, behind what the call's queue already holds (ADR-0010 X2). A
//! deferred arming tells the router loop how it ended ([`arming_ended_of`]):
//! it ran, or it met the node's own endpoint not reading ready and left the
//! call parked, to be armed with the gate's next release (ADR-0055 §8).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use super::super::materialise::{arm, caller_invite_key, materialise, Materialised, Origin};
use super::super::release::{release_call, ReleaseKind};
use super::super::undecided::{is_undecided, settle_undecided};
use super::super::RouterCtx;
use super::bulk::{announce_reclaim, smoothing};
use b2bua_sdk::event::CallEvent;

/// The topic of a deferred step's notice, an internal event the router raises
/// itself and its turn consumes before any rule reads it.
const TOPIC: &str = "deferred-reclaim";
/// The notice's outcome for a bulk park.
const PARK: &str = "park";
/// The notice's outcome for the arming of a parked call; the cohort's largest
/// past-due keepalive gap rides the payload.
const ARM: &str = "arm";
/// The topic of the re-entry notice a deferred arming sends the router loop
/// as it ends.
const ARMING_ENDED: &str = "deferred-arming-ended";
/// That notice's outcome: the arming ran (the call is armed, or gone).
const RAN: &str = "ran";
/// That notice's outcome: the endpoint did not read ready, the call is left
/// parked.
const HELD: &str = "held";

/// How a deferred arming ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::router) enum ArmingEnded {
    /// It ran: the call is armed, or gone.
    Ran,
    /// The node's own endpoint did not read ready: the call is left parked.
    Held,
}

/// Queue the bulk park of `call_ref` on its own queue; whether it was queued.
pub(in crate::router) fn offer_park(ctx: &Arc<RouterCtx>, call_ref: &str) -> bool {
    queue(ctx, notice(call_ref, TOPIC, PARK, serde_json::Value::Null))
}

/// Queue the arming of the parked `call_ref`, its keepalives smoothed over a
/// cohort whose largest past-due gap is `l_max` ms; whether it was queued.
pub(in crate::router) fn offer_arm(ctx: &Arc<RouterCtx>, call_ref: &str, l_max: i64) -> bool {
    queue(ctx, notice(call_ref, TOPIC, ARM, serde_json::json!(l_max)))
}

fn queue(ctx: &Arc<RouterCtx>, event: CallEvent) -> bool {
    super::super::ingress::queue_notice(ctx, event)
}

fn notice(call_ref: &str, topic: &str, outcome: &str, payload: serde_json::Value) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref: call_ref.to_string(),
        topic: topic.to_string(),
        outcome: outcome.to_string(),
        payload,
        body: Vec::new(),
        incarnation: None,
    }
}

/// Whether `event` is a deferred step's notice, or the re-entry notice of an
/// arming that ended.
pub(in crate::router) fn is_notice(event: &CallEvent) -> bool {
    matches!(event, CallEvent::InternalEvent { topic, .. }
        if topic == TOPIC || topic == ARMING_ENDED)
}

/// Whether `event` is the re-entry notice of a deferred arming that ended:
/// of which call, and how.
pub(in crate::router) fn arming_ended_of(event: &CallEvent) -> Option<(&str, ArmingEnded)> {
    match event {
        CallEvent::InternalEvent { call_ref, topic, outcome, .. } if topic == ARMING_ENDED => {
            let ended = if outcome == HELD { ArmingEnded::Held } else { ArmingEnded::Ran };
            Some((call_ref, ended))
        }
        _ => None,
    }
}

/// The turn of a deferred step's notice `event`. Boxed: a deferred step is
/// rare, and the turn body that runs every event stays small.
pub(in crate::router) fn deferred_turn<'a>(
    ctx: &'a Arc<RouterCtx>,
    event: &'a CallEvent,
) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        let CallEvent::InternalEvent { call_ref, topic, outcome, payload, .. } = event else {
            return;
        };
        if topic != TOPIC {
            return;
        }
        match outcome.as_str() {
            PARK => park_turn(ctx, call_ref).await,
            ARM => arm_turn(ctx, call_ref, payload.as_i64().unwrap_or_default()).await,
            _ => {}
        }
    })
}

/// The bulk park of `call_ref`, as the bulk pass takes it (`bulk::park_all`).
async fn park_turn(ctx: &Arc<RouterCtx>, call_ref: &str) {
    {
        let _guard = ctx.state.lock(call_ref).await;
        if let Materialised::Parked(call) = materialise(ctx, call_ref, Origin::Park).await {
            announce_reclaim(ctx, call);
        }
    }
    release_if_left(ctx, call_ref).await;
}

/// The arming of the parked `call_ref`, its smoothing read at the turn's
/// instant, as the arming pass takes it (`bulk::arm_parked`), while the node's
/// own endpoint reads ready; otherwise the call is left parked. A copy of the
/// caller's INVITE that reached the call meanwhile kept the transaction the
/// layer opened for it: while that transaction has sent no final and taken
/// no CANCEL, it gives way, with nothing sent, to the one the arming
/// rebuilds, and the copy, queued behind, meets the rebuilt one. One that
/// took a CANCEL keeps its key and the 487 it holds; the arming seeds nothing
/// in its place, and the undecided call ends asking nothing.
async fn arm_turn(ctx: &Arc<RouterCtx>, call_ref: &str, l_max: i64) {
    let mut ended = ArmingRan { ctx, call_ref, ended: ArmingEnded::Ran };
    {
        let _guard = ctx.state.lock(call_ref).await;
        let ready = ctx.readiness.own_endpoint_ready().is_none_or(|ready| *ready.borrow());
        if !ready {
            ended.ended = ArmingEnded::Held;
            return;
        }
        if ctx.state.is_parked(call_ref) {
            if let Some(key) = ctx.state.peek(call_ref).as_ref().and_then(caller_invite_key) {
                let _ = ctx.txn.relinquish_unanswered_server(key).await;
            }
        }
        let smoothing = smoothing(ctx, ctx.clock.now_ms(), l_max);
        if let Some((call, seeded)) = arm(ctx, call_ref, Some(smoothing)).await {
            if let Some(seeded) = seeded.filter(|_| is_undecided(&call)) {
                settle_undecided(ctx, call_ref, call, seeded).await;
            }
        }
    }
    release_if_left(ctx, call_ref).await;
}

/// A turn that leaves no call live here and nothing queued behind it releases
/// the queue it opened, its guard dropped first so a step deferred behind it
/// runs before the release.
async fn release_if_left(ctx: &Arc<RouterCtx>, call_ref: &str) {
    if !ctx.state.contains(call_ref) && ctx.dispatcher.is_idle(call_ref) {
        release_call(ctx, call_ref, ReleaseKind::Orphan).await;
    }
}

/// Tells the router loop how the deferred arming of `call_ref` ended, however
/// its turn ends.
struct ArmingRan<'a> {
    ctx: &'a RouterCtx,
    call_ref: &'a str,
    ended: ArmingEnded,
}

impl Drop for ArmingRan<'_> {
    fn drop(&mut self) {
        let outcome = match self.ended {
            ArmingEnded::Ran => RAN,
            ArmingEnded::Held => HELD,
        };
        let notice = notice(self.call_ref, ARMING_ENDED, outcome, serde_json::Value::Null);
        let _ = self.ctx.reentry_tx.send(notice);
    }
}
