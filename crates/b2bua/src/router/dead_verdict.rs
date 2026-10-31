//! A front proxy's dead verdict on another worker (ADR-0050 §3, ADR-0047 D1):
//! the `X-Worker-Dead` notice an out-of-dialog OPTIONS carries. The proxy now
//! routes that worker's traffic here, so this node takes over the calls it
//! backs for it that have work in flight, as the departure sweep does, and
//! releases the limiter keys its ended calls owe, writing none of their CDRs
//! ([`super::departure`]).
//!
//! A notice is taken only when it verifies under the cluster key and its
//! verdict is fresh: within the freshness window of this node's clock either
//! way ([`VerdictBounds`](crate::repl::VerdictBounds)), so a
//! replayed notice is refused once that window passes. A node with no cluster
//! key trusts none. Each verdict is taken once: a copy of one already taken
//! (a Timer E repeat, up to Timer F, or a second proxy's own verdict at the
//! same instant) is a duplicate, neither checked again nor refused; a later
//! verdict on the same worker sweeps again. Only the first notice of a
//! request is read. A notice naming this node is ignored: a proxy that reads
//! it dead routes its calls to their backups.
//!
//! A taken verdict acts once this node's replication stream from that worker
//! is down: at once when it already is, else when it ends within the stream's
//! silence bound with no frame from the worker since the verdict. A frame
//! after the verdict, or a stream open through that bound, speaks for a live
//! worker the proxy misread, and nothing is taken over (ADR-0047 D1); one wait
//! runs per worker.
//!
//! A worker whose verdict acted reads dead here until a frame from it arrives
//! ([`primary_read_dead`]), which clears the verdict: a call a copy here ends
//! meanwhile releases its limiter key at its end, and its CDR waits for the
//! worker's departure or reconcile (ADR-0047 D8).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tokio::time::Instant;

use sip_ids::SigningKey;
use sip_message::dead_notice::{dead_notice, ReceivedNotice};
use sip_message::SipRequest;

use super::departure::{backs_calls_of, take_over_read_dead};
use super::RouterCtx;
use crate::poison::{locked, DEAD_VERDICTS};
use crate::repl::ReplicationSupervisor;

/// The verdicts this node took, by worker: each instant, held for the hold
/// of [`VerdictBounds`](crate::repl::VerdictBounds).
#[derive(Default)]
pub(crate) struct VerdictBook {
    taken: Mutex<HashMap<String, BTreeSet<i64>>>,
    /// The workers whose verdict waits on the stream's confirmation: one
    /// wait per worker, however many verdicts on it arrive meanwhile.
    pending: Mutex<HashSet<String>>,
    /// The workers whose verdict acted, each at the instant of its latest.
    acted: Mutex<HashMap<String, Instant>>,
}

impl VerdictBook {
    /// Whether the verdict on `ordinal` at `at_ms` is held, dropping every
    /// one past `hold_ms` at `now_ms`.
    fn holds(&self, ordinal: &str, at_ms: i64, now_ms: i64, hold_ms: i64) -> bool {
        let mut taken = locked(&self.taken, DEAD_VERDICTS);
        taken.retain(|_, ats| {
            ats.retain(|at| now_ms.saturating_sub(*at) <= hold_ms);
            !ats.is_empty()
        });
        taken.get(ordinal).is_some_and(|ats| ats.contains(&at_ms))
    }

    /// Mark a confirmation pending for `ordinal`; whether none was.
    fn start_pending(&self, ordinal: &str) -> bool {
        locked(&self.pending, DEAD_VERDICTS).insert(ordinal.to_string())
    }

    /// The confirmation pending for `ordinal` ended.
    fn end_pending(&self, ordinal: &str) {
        locked(&self.pending, DEAD_VERDICTS).remove(ordinal);
    }

    /// Hold the verdict on `ordinal` at `at_ms`; whether it was not held.
    fn take(&self, ordinal: &str, at_ms: i64) -> bool {
        locked(&self.taken, DEAD_VERDICTS).entry(ordinal.to_string()).or_default().insert(at_ms)
    }

    /// A verdict on `ordinal` acted now.
    fn acted(&self, ordinal: &str) {
        locked(&self.acted, DEAD_VERDICTS).insert(ordinal.to_string(), Instant::now());
    }

    /// The instant the latest verdict on `ordinal` acted, if one did.
    fn acted_at(&self, ordinal: &str) -> Option<Instant> {
        locked(&self.acted, DEAD_VERDICTS).get(ordinal).copied()
    }

    /// Forget the verdict on `ordinal` that acted at `at`; a later one stays.
    fn heard_since(&self, ordinal: &str, at: Instant) {
        let mut acted = locked(&self.acted, DEAD_VERDICTS);
        if acted.get(ordinal) == Some(&at) {
            acted.remove(ordinal);
        }
    }
}

/// Whether `call_ref`'s primary is another worker a verdict here acted on, and
/// no frame from it arrived since. The first frame read after the verdict
/// clears it: a puller started afresh reads no frame before it.
pub(super) fn primary_read_dead(ctx: &RouterCtx, call_ref: &str) -> bool {
    let Some(primary) = call::call_ref_primary(call_ref) else { return false };
    if primary == ctx.config.self_ordinal {
        return false;
    }
    let Some(at) = ctx.dead_verdicts.acted_at(primary) else { return false };
    let heard = ctx.peer_streams.as_ref().is_some_and(|s| s.heard_from_since(primary, at));
    if heard {
        ctx.dead_verdicts.heard_since(primary, at);
    }
    !heard
}

/// Why a notice is not taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    NoKey,
    Unsigned,
    BadSignature,
    Stale,
}

/// Take the dead notice `req` carries, if any.
pub(super) fn on_options(ctx: &Arc<RouterCtx>, req: &SipRequest) {
    if let Some(received) = dead_notice(req) {
        take(ctx, &received);
    }
}

fn take(ctx: &Arc<RouterCtx>, received: &ReceivedNotice) {
    let now_ms = ctx.clock.now_ms();
    let notice = &received.notice;
    let bounds = ctx.verdict_bounds;
    if ctx.dead_verdicts.holds(&notice.ordinal, notice.at_ms, now_ms, bounds.hold_ms) {
        return;
    }
    if let Err(refusal) = verify(ctx.config.cluster_key.as_ref(), received, now_ms, bounds.fresh_ms)
    {
        ctx.metrics.bump_dead_notice_refused();
        tracing::debug!(
            node = observe::node(),
            worker = %notice.ordinal,
            at_ms = notice.at_ms,
            ?refusal,
            "dead notice refused"
        );
        return;
    }
    if notice.ordinal == ctx.config.self_ordinal {
        tracing::info!(node = observe::node(), "a proxy reads this node dead");
        return;
    }
    if !ctx.dead_verdicts.take(&notice.ordinal, notice.at_ms)
        || !backs_calls_of(ctx, &notice.ordinal)
    {
        return;
    }
    ctx.metrics.bump_dead_verdict_taken();
    match &ctx.peer_streams {
        Some(streams) if streams.streams_from(&notice.ordinal) => {
            if ctx.dead_verdicts.start_pending(&notice.ordinal) {
                confirm_on_the_stream(ctx, streams.clone(), notice.ordinal.clone(), notice.at_ms);
            }
        }
        _ => act(ctx, &notice.ordinal, notice.at_ms),
    }
}

/// Read `ordinal` dead and take over the calls this node backs for it with
/// work in flight.
fn act(ctx: &Arc<RouterCtx>, ordinal: &str, at_ms: i64) {
    ctx.dead_verdicts.acted(ordinal);
    if !take_over_read_dead(ctx, ordinal) {
        return;
    }
    tracing::info!(
        node = observe::node(),
        worker = %ordinal,
        at_ms,
        "a proxy reads the worker dead, its replication stream is down: its in-flight calls are taken over"
    );
}

/// Act on the verdict on `ordinal` once the replication stream from it ends,
/// within its silence bound, with no frame from it since the verdict; a frame
/// from it after the verdict, or the stream open past that bound, makes the
/// verdict a misread.
fn confirm_on_the_stream(
    ctx: &Arc<RouterCtx>,
    streams: ReplicationSupervisor,
    ordinal: String,
    at_ms: i64,
) {
    let task_ctx = ctx.clone();
    let confirm: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> = Box::pin(
        async move {
            let bound = streams.silence_bound();
            let since = tokio::time::Instant::now();
            let ends = streams.stream_from_ends_unheard_since(&ordinal, since);
            let confirmed = tokio::time::timeout(bound, ends).await.unwrap_or(false);
            task_ctx.dead_verdicts.end_pending(&ordinal);
            if confirmed {
                act(&task_ctx, &ordinal, at_ms);
                return;
            }
            task_ctx.metrics.bump_dead_verdict_unconfirmed();
            tracing::info!(
                node = observe::node(),
                worker = %ordinal,
                at_ms,
                "a proxy reads the worker dead while its replication stream lives: nothing is taken over"
            );
        },
    );
    let task = observe::spawn_keeping_node(confirm);
    ctx.dispatcher.track(task.abort_handle());
}

/// Whether `received` verifies under `key` and is fresh at `now_ms`, within
/// `fresh_ms` either way.
fn verify(
    key: Option<&SigningKey>,
    received: &ReceivedNotice,
    now_ms: i64,
    fresh_ms: i64,
) -> Result<(), Refusal> {
    let key = key.ok_or(Refusal::NoKey)?;
    let signature = received.signature.as_deref().ok_or(Refusal::Unsigned)?;
    let fields = received.notice.signed_fields();
    if !key.verify(&fields.each_ref().map(Vec::as_slice), signature) {
        return Err(Refusal::BadSignature);
    }
    if now_ms.saturating_sub(received.notice.at_ms).saturating_abs() > fresh_ms {
        return Err(Refusal::Stale);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
