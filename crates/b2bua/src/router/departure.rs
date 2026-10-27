//! The departure sweep (ADR-0047, ADR-0055 §5): a peer that left the
//! membership set is gone, and a party it left mid-transaction may send
//! nothing more — a caller that heard a 100 runs no Timer B (RFC 3261
//! §17.1.1.2), a silent callee stays silent, nobody else repeats a 2xx the
//! caller lost (§13.3.1.4), and nobody else ends a call the peer was ending.
//! Every call this node backs up for the peer with an INVITE transaction to
//! serve, and every `Terminating` one with any request record open — its own
//! BYE in flight, a request it held — or a caller dialog held possibly
//! confirmed (ADR-0055 §4), is taken over at once: its timers run
//! at their replicated deadlines, its transactions are rebuilt, and an
//! undecided call is answered. A `Terminating` call with nothing left to serve
//! finished its ending on the peer, whose last write was lost: it is
//! discharged, its legs forced terminal, with its CDR, which may be the
//! second (ADR-0055 §5). Any other call is left to the reactive path by user
//! ruling (ADR-0014 §1, ADR-0055 §5): a party's retransmission or a final that
//! fails over takes it over, and taking every quiet dialog over at once is the
//! stale-CSeq storm that rule exists to avoid; a call whose parties stay
//! silent ends at their own timers or the replica's TTL. A call a takeover
//! copy here ended and deferred to the peer, which left without reconciling
//! it, is discharged — CDR, limiter release, delete — since no node is left
//! to (ADR-0047 D8).
//!
//! The sweep is a task of its own, never the router loop's: a departed
//! partition holds every call the peer served, read one body at a time, and
//! the router serves its own calls meanwhile. The dispatcher tracks it, so a
//! crash of the node stops it with the call work it runs. Each call is taken under its
//! per-call lock, so the sweep and the call's own turns never interleave, and
//! in a subtask of its own: a call whose takeover panics costs that call only,
//! counted, and the sweep goes on with the next.

use std::future::Future;
use std::sync::Arc;

use call::{Call, CallModelState};

use super::materialise::{
    has_invite_to_serve, has_work_to_serve, materialise, Materialised, Origin,
};
use super::reclaim::discharge_materialized_terminal;
use super::restore_hygiene::reanchor;
use super::undecided::{is_undecided, settle_undecided};
use super::RouterCtx;
use crate::abort_on_drop::AbortOnDrop;
use crate::rules::actions::possibly_confirmed;
use crate::store::PartitionRole;

/// How many calls the sweep reads between two yields to the runtime.
const YIELD_EVERY: usize = 32;

/// Start the sweep of `ordinal`'s backup partition and return at once.
pub(super) fn take_over_departed(ctx: &Arc<RouterCtx>, ordinal: &str) {
    let refs = ctx.state.partition_refs(PartitionRole::Backup, ordinal);
    let (task_ctx, ordinal) = (ctx.clone(), ordinal.to_string());
    let task = observe::spawn_keeping_node(async move {
        sweep(&task_ctx, &ordinal, refs, |ctx, call_ref| async move {
            take_over_one(&ctx, &call_ref).await
        })
        .await;
    });
    ctx.dispatcher.track(task.abort_handle());
}

/// What the sweep did with one call.
enum Swept {
    /// Nothing in flight, already live here, or gone.
    Left,
    Taken,
    TakenAndSettled,
    /// Ended here and deferred to the peer: discharged.
    Discharged,
}

/// Run `step` over every call of `refs`, one at a time, each in a subtask of
/// its own: a step that panics is counted and logged, and the next call is
/// taken.
async fn sweep<F, Fut>(ctx: &Arc<RouterCtx>, ordinal: &str, refs: Vec<String>, step: F)
where
    F: Fn(Arc<RouterCtx>, String) -> Fut,
    Fut: Future<Output = Swept> + Send + 'static,
{
    let scanned = refs.len();
    let (mut taken, mut settled, mut discharged, mut panicked) = (0usize, 0usize, 0usize, 0usize);
    for (i, call_ref) in refs.into_iter().enumerate() {
        if i % YIELD_EVERY == YIELD_EVERY - 1 {
            tokio::task::yield_now().await;
        }
        let mut run = AbortOnDrop(observe::spawn_keeping_node(step(ctx.clone(), call_ref.clone())));
        // A crash of the node stops the call in flight with the sweep.
        ctx.dispatcher.track(run.0.abort_handle());
        match (&mut run.0).await {
            Ok(Swept::Left) => {}
            Ok(Swept::Taken) => taken += 1,
            Ok(Swept::TakenAndSettled) => (taken, settled) = (taken + 1, settled + 1),
            Ok(Swept::Discharged) => discharged += 1,
            Err(e) if e.is_panic() => {
                panicked += 1;
                ctx.metrics.bump_departure_takeover_panic();
                tracing::error!(
                    node = observe::node(),
                    peer = ordinal,
                    call_ref,
                    "a departed call's takeover panicked; the sweep goes on"
                );
            }
            Err(_) => {}
        }
    }
    tracing::info!(
        node = observe::node(),
        peer = ordinal,
        scanned,
        taken,
        settled,
        discharged,
        panicked,
        "departed peer's in-flight calls taken over"
    );
}

/// Discharge `call_ref` when its body is ended, or ending with nothing left to
/// serve, else take it over when it names an INVITE transaction to serve
/// ([`has_invite_to_serve`]), or is ending with work left ([`ending_work`]).
/// The body is read once without the lock to leave a
/// quiet call untouched; a call already live here is left as it is.
async fn take_over_one(ctx: &Arc<RouterCtx>, call_ref: &str) -> Swept {
    let Ok((copy, _skew)) = ctx.state.peek_replica(call_ref).await else { return Swept::Left };
    let call = copy.call;
    if finished(&call) {
        return discharge_finished(ctx, call_ref).await;
    }
    let ending = call.state == CallModelState::Terminating;
    if !has_invite_to_serve(&call) && !(ending && ending_work(&call)) {
        return Swept::Left;
    }
    let guard = ctx.state.lock(call_ref).await;
    match materialise(ctx, call_ref, Origin::Takeover).await {
        Materialised::Served { call, seeded } => match seeded {
            Some(seeded) if is_undecided(&call) => {
                settle_undecided(ctx, call_ref, call, seeded).await;
                Swept::TakenAndSettled
            }
            _ => Swept::Taken,
        },
        Materialised::Resident(_) | Materialised::Parked(_) => Swept::Left,
        // Gone from the partition since it was read: the lock entry this pass
        // made is the only trace, and no release will clear it. The guard
        // goes first, as it is held on that entry.
        Materialised::Refused(_) => {
            drop(guard);
            ctx.state.discard_orphan(call_ref);
            Swept::Left
        }
    }
}

/// Whether `call` needs no copy to serve it, only its discharge: ended, or
/// ending with no work left.
fn finished(call: &Call) -> bool {
    match call.state {
        CallModelState::Terminated => true,
        CallModelState::Terminating => !ending_work(call),
        _ => false,
    }
}

/// What an ending call still waits on before it ends: a transaction to serve
/// ([`has_work_to_serve`]), or a caller dialog held possibly confirmed, BYEd
/// on her ACK or when its hold ends.
fn ending_work(call: &Call) -> bool {
    has_work_to_serve(call) || possibly_confirmed::holds_any(call)
}

/// Discharge the finished `call_ref` — a takeover copy here ended it and
/// deferred it to the peer, or the peer's last write of its ending was lost —
/// since no one else discharges it now. The body is read again under the
/// lock, which the call's own turns take: one of them may have deleted it
/// since the sweep's read. An ending body's legs are forced terminal.
async fn discharge_finished(ctx: &Arc<RouterCtx>, call_ref: &str) -> Swept {
    let guard = ctx.state.lock(call_ref).await;
    let ended = match ctx.state.peek_replica(call_ref).await {
        Ok((mut ended, skew)) if finished(&ended.call) => {
            reanchor(&mut ended.call, skew);
            ended
        }
        _ if ctx.state.peek(call_ref).is_some() => return Swept::Left,
        Ok(_) => return Swept::Left,
        Err(_) => {
            drop(guard);
            ctx.state.discard_orphan(call_ref);
            return Swept::Left;
        }
    };
    let now_ms = ctx.clock.now_ms();
    let force_terminal = ended.call.state == CallModelState::Terminating;
    if discharge_materialized_terminal(ctx, call_ref, ended, now_ms, force_terminal).await {
        return Swept::Discharged;
    }
    Swept::Left
}

#[cfg(test)]
mod tests;
