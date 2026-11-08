//! The departure sweep (ADR-0047, ADR-0055 §5): a peer that left the
//! membership set is gone, and a party it left mid-transaction may send
//! nothing more — a caller that heard a 100 runs no Timer B (RFC 3261
//! §17.1.1.2), a silent callee stays silent, nobody else repeats a 2xx the
//! caller lost (§13.3.1.4), and nobody else ends a call the peer was ending.
//! Every call this node backs up for the peer with an INVITE transaction to
//! serve or an in-dialog request in flight but the keepalive, every `Active`
//! one awaiting the answer to a request it sent off its turn,
//! which died with the peer's task (ADR-0039), and every `Terminating` one
//! with any request record open — its own BYE in flight, a request it held —
//! or a caller dialog held possibly confirmed (ADR-0055 §4), is taken over at
//! once: its timers run
//! at their replicated deadlines, its transactions are rebuilt, its awaited
//! requests are sent again, and an undecided call's decision is asked again. A `Terminating` call with nothing left to serve
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
//! A proxy's dead verdict on the peer (ADR-0050 §3), once the replication
//! stream from the peer confirms it ([`super::dead_verdict`]), runs the
//! takeover half alone: the same calls are taken over at once, an undecided
//! one included, while a finished call and a deferred end are left to the
//! peer, which may live behind a partition and discharge them itself, or to
//! its departure; the final a finished call may still owe its caller is sent
//! at once, and the limiter key it owes is released (ADR-0047 D8): the
//! release is keyed by the call, so the peer's own release, or the
//! discharge's, frees nothing more. That half stops at the first write of a
//! later generation than the verdict named: the restarted peer holds its calls
//! again, and a copy read from a replica its reclaim write already reached meets
//! no later write to stand down on (ADR-0047 D1, D7).
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

use super::ended_body;
use super::materialise::{
    has_invite_to_serve, has_work_to_serve, materialise, names_request_in_flight, Materialised,
    Origin,
};
use super::restore_hygiene::reanchor;
use super::undecided::{is_undecided, settle_undecided};
use super::RouterCtx;
use crate::abort_on_drop::AbortOnDrop;
use crate::rules::actions::possibly_confirmed;
use crate::store::PartitionRole;

/// Whether `call_ref`'s primary is another node that has departed.
pub(super) fn primary_departed(ctx: &RouterCtx, call_ref: &str) -> bool {
    match (&ctx.departed, call::call_ref_primary(call_ref)) {
        (Some(departed), Some(primary)) => primary != ctx.config.self_ordinal && departed(primary),
        _ => false,
    }
}

/// How many calls the sweep reads between two yields to the runtime.
const YIELD_EVERY: usize = 32;

/// Why the peer's calls are swept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Cause {
    /// The peer left the membership set: its calls are taken over and its
    /// finished ones discharged.
    Departed,
    /// A proxy read the peer dead: its calls are taken over, and its finished
    /// ones release their limiter keys, with no CDR. `generation` is the
    /// incarnation the verdict named, when the flow from the peer heard one.
    ReadDead { generation: Option<u64> },
}

/// Start the sweep of `ordinal`'s backup partition and return at once.
pub(super) fn take_over_departed(ctx: &Arc<RouterCtx>, ordinal: &str) {
    start(ctx, ordinal, Cause::Departed);
}

/// Start the takeover half of the sweep of `ordinal`'s backup partition, on a
/// proxy's dead verdict, and return at once; whether this node backs any call
/// for `ordinal`, and so swept anything. `generation` is the incarnation the
/// verdict named.
pub(super) fn take_over_read_dead(
    ctx: &Arc<RouterCtx>,
    ordinal: &str,
    generation: Option<u64>,
) -> bool {
    start(ctx, ordinal, Cause::ReadDead { generation })
}

/// Whether this node backs any call for `ordinal`.
pub(super) fn backs_calls_of(ctx: &RouterCtx, ordinal: &str) -> bool {
    !ctx.state.partition_refs(PartitionRole::Backup, ordinal).is_empty()
}

/// Start the sweep for `cause` unless `ordinal`'s backup partition is empty;
/// whether it started.
fn start(ctx: &Arc<RouterCtx>, ordinal: &str, cause: Cause) -> bool {
    let mut refs = ctx.state.partition_refs(PartitionRole::Backup, ordinal);
    refs.sort_unstable();
    if refs.is_empty() && matches!(cause, Cause::ReadDead { .. }) {
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
    /// Finished, the peer read dead: its limiter key released.
    Released,
    /// The restarted peer writes again: this call and the rest are its own.
    PeerReturned,
}

/// Run `step` over the calls of `refs`, one at a time, each in a subtask of
/// its own: a step that panics is counted and logged, and the next call is
/// taken. A step that finds the peer returned ends the run.
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
    let mut released = 0usize;
    for (i, call_ref) in refs.into_iter().enumerate() {
        if i % YIELD_EVERY == YIELD_EVERY - 1 {
            tokio::task::yield_now().await;
        }
        ctx.turn_probe.sweep_step(&ctx.config.self_ordinal).await;
        let mut run = AbortOnDrop(observe::spawn_keeping_node(step(ctx.clone(), call_ref.clone())));
        // A crash of the node stops the call in flight with the sweep.
        ctx.dispatcher.track(run.0.abort_handle());
        match (&mut run.0).await {
            Ok(Swept::Left) => {}
            Ok(Swept::Taken) => taken += 1,
            Ok(Swept::TakenAndSettled) => (taken, settled) = (taken + 1, settled + 1),
            Ok(Swept::Discharged) => discharged += 1,
            Ok(Swept::Released) => released += 1,
            Ok(Swept::PeerReturned) => {
                tracing::info!(
                    node = observe::node(),
                    peer = ordinal,
                    "the read-dead peer restarted and writes: the sweep stops, its calls are its own"
                );
                break;
            }
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
        Cause::ReadDead { .. } => tracing::info!(
            node = observe::node(),
            peer = ordinal,
            scanned,
            taken,
            settled,
            released,
            panicked,
            "read-dead peer's in-flight calls taken over"
        ),
    }
}

/// Discharge `call_ref` when its body is ended, or ending with nothing left to
/// serve, and the peer departed; else take it over when it names an INVITE
/// transaction to serve ([`has_invite_to_serve`]) or an in-dialog request in
/// flight ([`names_request_in_flight`]), is ending with work left
/// ([`ending_work`]), or, `Active`, awaits the answer to a request it sent off
/// its turn, which the copy sends again; an undecided call's decision is asked again
/// ([`settle_undecided`]). On a dead verdict a finished call is left to the
/// departure, the final it may still owe its caller sent
/// ([`super::owed_final`]), and the limiter key it owes released while the
/// peer still reads dead. The body is read once without the lock to leave a
/// quiet call untouched; a call already live here is left as it is. On a dead
/// verdict the flow from the peer is read before the call is touched, again
/// under the lock right before the copy reads the replica, and once the copy is
/// marked ([`peer_returned`]): a write heard after that mark rides up to the
/// copy, and one heard before it may have found no mark, so the copy is offered
/// the Element as that write
/// ([`offer_the_element`](super::stand_down::offer_the_element)).
async fn take_over_one(ctx: &Arc<RouterCtx>, call_ref: &str, cause: Cause) -> Swept {
    if peer_returned(ctx, call_ref, cause) {
        return Swept::PeerReturned;
    }
    let Ok((mut copy, skew)) = ctx.state.peek_replica(call_ref).await else { return Swept::Left };
    if finished(&copy.call) {
        return match cause {
            Cause::Departed => discharge_finished(ctx, call_ref).await,
            Cause::ReadDead { .. } => {
                reanchor(&mut copy.call, skew);
                let still = super::dead_verdict::primary_read_dead(ctx, call_ref);
                let owed = ended_body::Cause::ReadDead { still };
                let paid = ended_body::pay(ctx, call_ref, owed, copy, ctx.clock.now_ms()).await;
                if paid.released {
                    Swept::Released
                } else {
                    Swept::Left
                }
            }
        };
    }
    let call = copy.call;
    let ending = call.state == CallModelState::Terminating;
    let at_once = has_invite_to_serve(&call) || (ending && ending_work(&call));
    let awaited = call.state == CallModelState::Active && !call.awaited.is_empty();
    if !at_once && !names_request_in_flight(&call) && !awaited {
        return Swept::Left;
    }
    let _guard = ctx.state.lock(call_ref).await;
    if peer_returned(ctx, call_ref, cause) {
        return Swept::PeerReturned;
    }
    match materialise(ctx, call_ref, Origin::Takeover).await {
        Materialised::Served { .. } if peer_returned(ctx, call_ref, cause) => {
            super::stand_down::offer_the_element(ctx, call_ref).await;
            Swept::PeerReturned
        }
        Materialised::Served { call, seeded } => match seeded {
            Some(seeded) if is_undecided(&call) => {
                settle_undecided(ctx, call_ref, call, seeded).await;
                Swept::TakenAndSettled
            }
            _ => Swept::Taken,
        },
        // Live here already, or gone from the partition since it was read.
        Materialised::Resident(_) | Materialised::Parked(_) | Materialised::Refused(_) => {
            Swept::Left
        }
    }
}

/// Whether, on a dead verdict, the flow from `call_ref`'s primary heard a write
/// of a later generation than the verdict named. A frame is heard before it is
/// applied, so a reclaim write this reads `false` for is not in the replica yet.
fn peer_returned(ctx: &RouterCtx, call_ref: &str, cause: Cause) -> bool {
    let Cause::ReadDead { generation: Some(generation) } = cause else { return false };
    let Some(primary) = call::call_ref_primary(call_ref) else { return false };
    ctx.peer_streams.as_ref().is_some_and(|s| s.writes_a_later_generation(primary, generation))
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
    let _guard = ctx.state.lock(call_ref).await;
    let ended = match ctx.state.peek_replica(call_ref).await {
        Ok((mut ended, skew)) if finished(&ended.call) => {
            reanchor(&mut ended.call, skew);
            ended
        }
        _ => return Swept::Left,
    };
    let now_ms = ctx.clock.now_ms();
    if ended_body::pay(ctx, call_ref, ended_body::Cause::Departed, ended, now_ms).await.discharged {
        return Swept::Discharged;
    }
    Swept::Left
}

#[cfg(test)]
mod crossing_tests;
#[cfg(test)]
mod tests;
