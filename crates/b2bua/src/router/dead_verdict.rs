//! A front proxy's dead verdict on another worker (ADR-0050 §3, ADR-0047 D1):
//! the `X-Worker-Dead` notice an out-of-dialog OPTIONS carries. The proxy now
//! routes that worker's traffic here, so this node takes over the calls it
//! backs for it that have work in flight, as the departure sweep does, minus
//! the discharge ([`super::departure`]).
//!
//! A notice is taken only when it verifies under the cluster key and its
//! verdict is fresh: within [`FRESH_MS`] of this node's clock either way, so a
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
//! silence bound. A stream that stays open through that bound speaks for a
//! live worker the proxy misread, and nothing is taken over (ADR-0047 D1).

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use sip_ids::SigningKey;
use sip_message::dead_notice::{dead_notice, ReceivedNotice};
use sip_message::SipRequest;

use super::departure::{backs_calls_of, take_over_read_dead};
use super::RouterCtx;
use crate::poison::{locked, DEAD_VERDICTS};
use crate::repl::ReplicationSupervisor;

/// How far a verdict's instant may stand from this node's clock, either way.
pub(crate) const FRESH_MS: i64 = 10_000;

/// How long a taken verdict is held: past the freshness window by Timer F
/// (64·T1), the last a copy of its notice can arrive.
pub(crate) const HELD_MS: i64 = FRESH_MS + 32_000;

/// The verdicts this node took, by worker: each instant, held [`HELD_MS`].
#[derive(Default)]
pub(crate) struct VerdictBook {
    taken: Mutex<HashMap<String, BTreeSet<i64>>>,
}

impl VerdictBook {
    /// Whether the verdict on `ordinal` at `at_ms` is held, dropping every
    /// one past [`HELD_MS`] at `now_ms`.
    fn holds(&self, ordinal: &str, at_ms: i64, now_ms: i64) -> bool {
        let mut taken = locked(&self.taken, DEAD_VERDICTS);
        taken.retain(|_, ats| {
            ats.retain(|at| now_ms.saturating_sub(*at) <= HELD_MS);
            !ats.is_empty()
        });
        taken.get(ordinal).is_some_and(|ats| ats.contains(&at_ms))
    }

    /// Hold the verdict on `ordinal` at `at_ms`; whether it was not held.
    fn take(&self, ordinal: &str, at_ms: i64) -> bool {
        locked(&self.taken, DEAD_VERDICTS).entry(ordinal.to_string()).or_default().insert(at_ms)
    }
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
    if ctx.dead_verdicts.holds(&notice.ordinal, notice.at_ms, now_ms) {
        return;
    }
    if let Err(refusal) = verify(ctx.config.cluster_key.as_ref(), received, now_ms) {
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
            confirm_on_the_stream(ctx, streams.clone(), notice.ordinal.clone(), notice.at_ms);
        }
        _ => act(ctx, &notice.ordinal, notice.at_ms),
    }
}

/// Take over the calls this node backs for `ordinal` with work in flight.
fn act(ctx: &Arc<RouterCtx>, ordinal: &str, at_ms: i64) {
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
/// within its silence bound; past that bound the verdict is a misread.
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
            if tokio::time::timeout(bound, streams.stream_from_ends(&ordinal)).await.is_ok() {
                act(&task_ctx, &ordinal, at_ms);
                return;
            }
            task_ctx.metrics.bump_dead_verdict_unconfirmed();
            tracing::info!(
                node = observe::node(),
                worker = %ordinal,
                at_ms,
                "a proxy reads the worker dead while its replication stream stays open: nothing is taken over"
            );
        },
    );
    let task = observe::spawn_keeping_node(confirm);
    ctx.dispatcher.track(task.abort_handle());
}

/// Whether `received` verifies under `key` and is fresh at `now_ms`.
fn verify(key: Option<&SigningKey>, received: &ReceivedNotice, now_ms: i64) -> Result<(), Refusal> {
    let key = key.ok_or(Refusal::NoKey)?;
    let signature = received.signature.as_deref().ok_or(Refusal::Unsigned)?;
    let fields = received.notice.signed_fields();
    if !key.verify(&fields.each_ref().map(Vec::as_slice), signature) {
        return Err(Refusal::BadSignature);
    }
    if now_ms.saturating_sub(received.notice.at_ms).saturating_abs() > FRESH_MS {
        return Err(Refusal::Stale);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
