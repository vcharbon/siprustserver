//! The departure sweep (ADR-0047, ADR-0055 §5): a peer that left the
//! membership set is gone, and a party it left mid-transaction may send
//! nothing more — a caller that heard a 100 runs no Timer B (RFC 3261
//! §17.1.1.2), a silent callee stays silent, nobody else repeats a 2xx the
//! caller lost (§13.3.1.4), and nobody else ends a call the peer was ending.
//! Every call this node backs up for the peer with an INVITE transaction to
//! serve or an in-dialog request in flight but the keepalive, every one
//! awaiting the answer to a request it sent off its turn,
//! which died with the peer's task (ADR-0039), and every `Terminating` one
//! with any request record open — its own BYE in flight, a request it held —
//! or a caller dialog held possibly confirmed (ADR-0055 §4), is taken over at
//! once: its timers run
//! at their replicated deadlines, its transactions are rebuilt, its awaited
//! requests are sent again, and an undecided call is answered. A `Terminating` call with nothing left to serve
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
//! A proxy's dead verdict on the peer (ADR-0050 §3) runs the takeover half
//! alone: the same calls are taken over, while an undecided call, a finished
//! call and a deferred end are left to the peer, which may live behind a
//! partition and decide or discharge them itself, or to its departure. A call
//! whose only work is a request in flight — an outside answer it awaits, an
//! in-dialog request — is taken over only once those requests are due: a
//! primary read dead while it lives takes its own answers by then, and its
//! write takes them off the record, so a node-local re-check of that call at
//! the latest of them finds nothing to do.
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
    has_invite_to_serve, has_work_to_serve, materialise, names_request_in_flight, Materialised,
    Origin,
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

/// Why the peer's calls are swept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Cause {
    /// The peer left the membership set: its calls are taken over and its
    /// finished ones discharged.
    Departed,
    /// A proxy read the peer dead: its calls are taken over, nothing is
    /// discharged.
    ReadDead,
}

/// Start the sweep of `ordinal`'s backup partition and return at once.
pub(super) fn take_over_departed(ctx: &Arc<RouterCtx>, ordinal: &str) {
    start(ctx, ordinal, Cause::Departed);
}

/// Start the takeover half of the sweep of `ordinal`'s backup partition, on a
/// proxy's dead verdict, and return at once; whether this node backs any call
/// for `ordinal`, and so swept anything.
pub(super) fn take_over_read_dead(ctx: &Arc<RouterCtx>, ordinal: &str) -> bool {
    start(ctx, ordinal, Cause::ReadDead)
}

/// Start the sweep for `cause` unless `ordinal`'s backup partition is empty;
/// whether it started.
fn start(ctx: &Arc<RouterCtx>, ordinal: &str, cause: Cause) -> bool {
    let refs = ctx.state.partition_refs(PartitionRole::Backup, ordinal);
    if refs.is_empty() && cause == Cause::ReadDead {
        return false;
    }
    let (task_ctx, ordinal) = (ctx.clone(), ordinal.to_string());
    let task = observe::spawn_keeping_node(async move {
        sweep(&task_ctx, &ordinal, cause, refs, move |ctx, call_ref| async move {
            take_over_one(&ctx, &call_ref, cause).await
        })
        .await;
    });
    ctx.dispatcher.track(task.abort_handle());
    true
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
async fn sweep<F, Fut>(
    ctx: &Arc<RouterCtx>,
    ordinal: &str,
    cause: Cause,
    refs: Vec<String>,
    step: F,
) where
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
    match cause {
        Cause::Departed => tracing::info!(
            node = observe::node(),
            peer = ordinal,
            scanned,
            taken,
            settled,
            discharged,
            panicked,
            "departed peer's in-flight calls taken over"
        ),
        Cause::ReadDead => tracing::info!(
            node = observe::node(),
            peer = ordinal,
            scanned,
            taken,
            settled,
            panicked,
            "read-dead peer's in-flight calls taken over"
        ),
    }
}

/// Discharge `call_ref` when its body is ended, or ending with nothing left to
/// serve, and the peer departed; else take it over when it names an INVITE
/// transaction to serve ([`has_invite_to_serve`]) or an in-dialog request in
/// flight ([`names_request_in_flight`]), is ending with work left
/// ([`ending_work`]), or awaits the answer to a request it sent off its turn,
/// which the copy sends again. On a dead verdict an undecided call is left to
/// the departure: its decision may be in flight at a primary the proxy misread
/// dead; a call with only requests in flight is taken over once they are due
/// ([`requests_due_at`]), re-checked then ([`recheck_at`]). The body is read once without the lock to leave a quiet call
/// untouched; a call already live here is left as it is.
async fn take_over_one(ctx: &Arc<RouterCtx>, call_ref: &str, cause: Cause) -> Swept {
    let Ok((copy, skew)) = ctx.state.peek_replica(call_ref).await else { return Swept::Left };
    let mut call = copy.call;
    if finished(&call) {
        return match cause {
            Cause::Departed => discharge_finished(ctx, call_ref).await,
            Cause::ReadDead => Swept::Left,
        };
    }
    let ending = call.state == CallModelState::Terminating;
    let at_once = has_invite_to_serve(&call) || (ending && ending_work(&call));
    let awaited = call.state == CallModelState::Active && !call.awaited.is_empty();
    if !at_once && !names_request_in_flight(&call) && !awaited {
        return Swept::Left;
    }
    if cause == Cause::ReadDead && is_undecided(&call) {
        return Swept::Left;
    }
    if cause == Cause::ReadDead && !at_once {
        reanchor(&mut call, skew);
        match requests_due_at(ctx, &call) {
            Some(due) if due <= ctx.clock.now_ms() => {}
            Some(due) => {
                recheck_at(ctx, call_ref, due);
                return Swept::Left;
            }
            None => return Swept::Left,
        }
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

/// When the requests `call` has in flight are all due — each outside
/// request's answer deadline, a REFER's authorization's send time plus the
/// decision deadline and its margin, each in-dialog request's Timer F or
/// answer deadline — the latest of them; `None` when one has no bound. A
/// primary read dead while it lives has taken each answer by then, and its
/// write took the request off the record.
fn requests_due_at(ctx: &RouterCtx, call: &Call) -> Option<i64> {
    let decision = u64::try_from(ctx.config.call_control_timeout_ms).ok().filter(|ms| *ms > 0);
    let margin = call::millis_i64(crate::answer_deadline::MARGIN);
    let awaited = call.awaited.iter().map(|r| match r {
        call::OffTurnRequest::Refer { sent_at_ms, .. } => {
            decision.map(|d| sent_at_ms + i64::try_from(d).unwrap_or(i64::MAX) + margin)
        }
        request => super::resend::deadline_of(call, request),
    });
    let in_dialog = call::helpers::every_record(call)
        .filter(|(_, _, r)| {
            r.method != "INVITE" && r.author.as_deref() != Some(call::KEEPALIVE_AUTHOR)
        })
        .map(|(_, _, r)| r.deadline_ms);
    awaited.chain(in_dialog).try_fold(i64::MIN, |due, at| at.map(|at| due.max(at)))
}

/// Take `call_ref` over at `due_ms` if it still needs a copy then: a re-check
/// of one call, on this node alone, of the takeover half of a dead verdict.
fn recheck_at(ctx: &Arc<RouterCtx>, call_ref: &str, due_ms: i64) {
    let wait = u64::try_from(due_ms.saturating_sub(ctx.clock.now_ms())).unwrap_or(0);
    let (task_ctx, call_ref) = (ctx.clone(), call_ref.to_string());
    let recheck: std::pin::Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(async move {
        tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
        take_over_one(&task_ctx, &call_ref, Cause::ReadDead).await;
    });
    let task = observe::spawn_keeping_node(recheck);
    ctx.dispatcher.track(task.abort_handle());
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
        _ if ctx.state.contains(call_ref) => return Swept::Left,
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
