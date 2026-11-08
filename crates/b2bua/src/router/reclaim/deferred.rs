//! The reclaim steps a sweep defers to a turn of the call's own queue when a
//! turn holds the call's lock, so the sweep never waits on one: the bulk
//! reclaim's park, the arming of a deferred park, and the end of a ghost the
//! replica reap evicted. Each runs under the call's lock as the sweep would
//! have, behind what the call's queue already holds (ADR-0010 X2). A deferred
//! arming tells the router loop once it ran ([`armed_notice_of`]).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use call::MsgpackCodec;

use super::super::ended_body::{self, Cause};
use super::super::materialise::{arm, materialise, Materialised, Origin};
use super::super::release::{release_call, ReleaseKind};
use super::super::undecided::{is_undecided, settle_undecided};
use super::super::RouterCtx;
use super::bulk::{announce_reclaim, smoothing};
use crate::store::{envelope, Enveloped};
use b2bua_sdk::event::CallEvent;

/// The topic of a deferred step's notice, an internal event the router raises
/// itself and its turn consumes before any rule reads it.
const TOPIC: &str = "deferred-reclaim";
/// The notice's outcome for a bulk park.
const PARK: &str = "park";
/// The notice's outcome for the arming of a parked call; the cohort's largest
/// past-due keepalive gap rides the payload.
const ARM: &str = "arm";
/// The notice's outcome for a ghost's end; the ghost's body rides the event.
const GHOST: &str = "ghost";
/// The topic of the re-entry notice a deferred arming sends the router loop
/// once it ran.
const ARMED: &str = "deferred-arm-ran";

/// Queue the bulk park of `call_ref` on its own queue; whether it was queued.
pub(in crate::router) fn offer_park(ctx: &Arc<RouterCtx>, call_ref: &str) -> bool {
    queue(ctx, notice(call_ref, TOPIC, PARK, serde_json::Value::Null, Vec::new()))
}

/// Queue the arming of the parked `call_ref`, its keepalives smoothed over a
/// cohort whose largest past-due gap is `l_max` ms; whether it was queued.
pub(in crate::router) fn offer_arm(ctx: &Arc<RouterCtx>, call_ref: &str, l_max: i64) -> bool {
    queue(ctx, notice(call_ref, TOPIC, ARM, serde_json::json!(l_max), Vec::new()))
}

/// Queue the end of `ghost`, which the replica reap evicted, on its call's
/// queue; whether it was queued.
pub(in crate::router) fn offer_ghost(ctx: &Arc<RouterCtx>, ghost: &Enveloped) -> bool {
    let call_ref = &ghost.call.call_ref;
    let body = match envelope::encode(&MsgpackCodec::new(), ghost.topology.as_ref(), &ghost.call) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!(node = observe::node(), call_ref, error = %e, "a ghost does not encode: it is left unended");
            return false;
        }
    };
    queue(ctx, notice(call_ref, TOPIC, GHOST, serde_json::Value::Null, body))
}

fn queue(ctx: &Arc<RouterCtx>, event: CallEvent) -> bool {
    super::super::ingress::queue_notice(ctx, event)
}

fn notice(
    call_ref: &str,
    topic: &str,
    outcome: &str,
    payload: serde_json::Value,
    body: Vec<u8>,
) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref: call_ref.to_string(),
        topic: topic.to_string(),
        outcome: outcome.to_string(),
        payload,
        body,
        incarnation: None,
    }
}

/// Whether `event` is a deferred step's notice, or the re-entry notice of an
/// arming that ran.
pub(in crate::router) fn is_notice(event: &CallEvent) -> bool {
    matches!(event, CallEvent::InternalEvent { topic, .. } if topic == TOPIC || topic == ARMED)
}

/// Whether `event` is the re-entry notice of a deferred arming that ran, and
/// of which call.
pub(in crate::router) fn armed_notice_of(event: &CallEvent) -> Option<&str> {
    match event {
        CallEvent::InternalEvent { call_ref, topic, .. } if topic == ARMED => Some(call_ref),
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
        let CallEvent::InternalEvent { call_ref, topic, outcome, payload, body, .. } = event else {
            return;
        };
        if topic != TOPIC {
            return;
        }
        match outcome.as_str() {
            PARK => park_turn(ctx, call_ref).await,
            ARM => arm_turn(ctx, call_ref, payload.as_i64().unwrap_or_default()).await,
            GHOST => ghost_turn(ctx, call_ref, body).await,
            _ => {}
        }
    })
}

/// The bulk park of `call_ref`, as the bulk pass takes it (`bulk::park_all`).
async fn park_turn(ctx: &Arc<RouterCtx>, call_ref: &str) {
    let _guard = ctx.state.lock(call_ref).await;
    if let Materialised::Parked(call) = materialise(ctx, call_ref, Origin::Park).await {
        announce_reclaim(ctx, call);
    }
    release_if_left(ctx, call_ref).await;
}

/// The arming of the parked `call_ref`, its smoothing read at the turn's
/// instant, as the arming pass takes it (`bulk::arm_parked`).
async fn arm_turn(ctx: &Arc<RouterCtx>, call_ref: &str, l_max: i64) {
    let _ran = ArmRan { ctx, call_ref };
    let _guard = ctx.state.lock(call_ref).await;
    let smoothing = smoothing(ctx, ctx.clock.now_ms(), l_max);
    if let Some((call, seeded)) = arm(ctx, call_ref, Some(smoothing)).await {
        if let Some(seeded) = seeded.filter(|_| is_undecided(&call)) {
            settle_undecided(ctx, call_ref, call, seeded).await;
        }
    }
    release_if_left(ctx, call_ref).await;
}

/// The end of the ghost `body` of `call_ref`, as the replica reap takes it
/// (`reap::reap_expired_replicas`).
async fn ghost_turn(ctx: &Arc<RouterCtx>, call_ref: &str, body: &[u8]) {
    let Some(ghost) = ctx.state.decode_body(body) else { return };
    let _guard = ctx.state.lock(call_ref).await;
    if !ctx.state.contains(call_ref) {
        ended_body::pay(ctx, call_ref, Cause::Ghost, ghost, ctx.clock.now_ms()).await;
    }
    release_if_left(ctx, call_ref).await;
}

/// A turn that leaves no call live here and nothing queued behind it releases
/// the queue it opened.
async fn release_if_left(ctx: &Arc<RouterCtx>, call_ref: &str) {
    if !ctx.state.contains(call_ref) && ctx.dispatcher.is_idle(call_ref) {
        release_call(ctx, call_ref, ReleaseKind::Orphan).await;
    }
}

/// Tells the router loop the deferred arming of `call_ref` ran, however its
/// turn ends.
struct ArmRan<'a> {
    ctx: &'a RouterCtx,
    call_ref: &'a str,
}

impl Drop for ArmRan<'_> {
    fn drop(&mut self) {
        let ran = notice(self.call_ref, ARMED, "", serde_json::Value::Null, Vec::new());
        let _ = self.ctx.reentry_tx.send(ran);
    }
}
