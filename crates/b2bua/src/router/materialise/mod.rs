//! Materialisation: a stored replica becomes the live copy this node serves,
//! and everything serving it needs is re-armed. One decision and one
//! post-materialise list for every entry path — the reactive takeover, the
//! on-demand reclaim, the bulk reboot sweep — so no path holds a hole another
//! closed.
//!
//! Per-call timers live in this node's `TimerService`, not in the replicated
//! call; a copy materialised from a replica arrives with no live timers here,
//! and its serialised intents (`call.timers`, which IS replicated) re-arm into
//! the local driver exactly once, on the materialisation that created the copy.
//! Each entry re-arms under its key and generation; a later rule-emitted
//! `ScheduleTimer` for the same key supersedes the restored entry.
//!
//! The caller holds the per-call lock. In order:
//! 1. residency — a call already live is handed back untouched
//!    ([`Materialised::Resident`]); its timers are live and never re-armed. A
//!    parked one is armed first ([`arm`]) and handed back as served;
//! 2. the store read the origin names — `bak:{primary}` for a takeover,
//!    `pri:{self}` for a reclaim — each miss a typed [`Reason`] the caller's
//!    orphan path answers (481 for a request, drop for a response);
//! 3. the state check — a takeover refuses a `Terminated` replica (the image of
//!    a call that already ended, kept for its primary to meet or reclaim;
//!    serving it would re-arm timers and run rules on a released copy) and
//!    serves a `Terminating` one, which still owes the teardown's end; a
//!    reclaim discharges a terminal body through the reclaim funnel instead of
//!    re-serving a dead dialog, `Terminating` forced terminal (ADR-0020 X3);
//! 4. `reanchor` — every instant of the copy moved into this node's clock
//!    frame by the persisted skew offset (ADR-0055 §9), right after the read;
//!    then `sanitize_restored_timers` drops the stale `KeepaliveTimeout`, and
//!    `ring_deadline::arm_unrung_after_restore` arms the ring deadline of an
//!    unanswered leg with no ring on record from its INVITE;
//! 5. the residency insert through the store; a parking reclaim stops here,
//!    the call parked unarmed until [`arm`] runs the rest;
//! 6. `timers.restore`;
//! 7. **seed** — the in-flight transactions the record names, the answered
//!    INVITEs whose 2xx it retains, and every non-INVITE request record's
//!    client and server halves ([`seeds::seeds_for`]), are rebuilt in the
//!    transaction layer, on every origin, so the layer owns their ACKs, their
//!    Timer D / E / G / L and the copies of their requests;
//! 8. takeover only: `mark_takeover` + `watch_self_release`, so the copy is
//!    shed once the transactions it serves — the seeds among them — clear
//!    (ADR-0014); the watch is armed after the seeds so it cannot fire on a
//!    copy that is about to hold one;
//! 9. metrics and the takeover episode line.
//!
//! The caller that holds a datagram then re-offers it ([`reoffer_trigger`]): a
//! response no client transaction matched, a CANCEL, or an INVITE the record
//! names is processed by the layer against the transactions it now holds, and
//! a match ends the turn — the layer's re-emission is the event the rules see.
//! The offer is made on every turn, not only the one that materialised: a
//! datagram the layer emitted between the trigger and the seeds queues behind
//! it on the per-call FIFO and finds the copy resident.
//!
//! An on-demand reclaim is not a takeover: the call is this node's own, so no
//! mark and no watch. A call never imported into `pri:{self}` (its only copy is
//! the peer's `bak:{self}`) is a genuine miss here.

mod received_request;
mod seeds;
pub(in crate::router) use seeds::{names_server_branch, relayed_server_key};
#[cfg(test)]
mod tests;

use std::sync::Arc;

use call::{Call, CallModelState};
use sip_message::{Method, SipMessage};
use sip_txn::Reoffer;

use super::reclaim::{discharge_materialized_terminal, format_pb};
use super::restore_hygiene::{reanchor, sanitize_restored_timers, Smoothing};
use super::RouterCtx;
use crate::store::{Enveloped, MaterialiseOrigin, ReplicaMiss};
use b2bua_sdk::event::CallEvent;

/// Which entry path is materialising: the whole axis of behaviour (spec D5).
#[derive(Debug, Clone, Copy)]
pub(super) enum Origin {
    /// An acting-backup takeover of a crashed peer's call, read from
    /// `bak:{primary}`: the copy is marked and watched for self-release.
    Takeover,
    /// This node re-serving its own call from `pri:{self}`, armed at once: a
    /// straggler or an on-demand reclaim.
    Reclaim,
    /// The bulk reboot reclaim's first pass over `pri:{self}`: the call is
    /// made resident and parked, nothing armed, so nothing of it leaves the
    /// node until [`arm`] runs (ADR-0055 §8).
    Park,
}

impl From<Origin> for MaterialiseOrigin {
    fn from(origin: Origin) -> Self {
        match origin {
            Origin::Takeover => MaterialiseOrigin::Takeover,
            Origin::Reclaim | Origin::Park => MaterialiseOrigin::Reclaim,
        }
    }
}

/// Why no call was served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reason {
    /// A takeover read of a ref this node is primary for.
    NotBackupRole,
    /// A reclaim read of a ref this node backs up.
    NotPrimaryRole,
    /// No body in the partition (never replicated, expired, or no replicating
    /// store wired).
    NoReplica,
    /// The body does not decode as a `Call`.
    Decode,
    /// A terminal body: refused on a takeover, discharged on a reclaim.
    Terminated,
    /// A body that does not rebuild its caller's INVITE, discharged
    /// (ADR-0052 §11).
    Unrecoverable,
}

/// The disposition of one [`materialise`] call.
#[derive(Debug)]
pub(super) enum Materialised {
    /// Freshly materialised on this call: inserted, timers re-armed, and for a
    /// takeover marked and watched. `seeded` counts the seeds the layer took
    /// — one whose branch a transaction here already held is not among them —
    /// and is `None` when the layer could not be asked (it has closed).
    Served { call: Call, seeded: Option<usize> },
    /// Already live on this node; nothing was re-armed.
    Resident(Call),
    /// Freshly made resident by a parking reclaim, nothing armed.
    Parked(Call),
    /// Not served; the caller's orphan path answers.
    Refused(Reason),
}

/// Materialise `call_ref` from the partition `origin` names (module doc). The
/// caller holds the per-call lock.
pub(super) async fn materialise(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    origin: Origin,
) -> Materialised {
    if let Some(live) = ctx.state.peek(call_ref) {
        return match arm(ctx, call_ref, None).await {
            Some((call, seeded)) => Materialised::Served { call, seeded },
            None => Materialised::Resident(live),
        };
    }
    let read = match origin {
        Origin::Takeover => ctx.state.peek_replica(call_ref).await,
        Origin::Reclaim | Origin::Park => ctx.state.peek_reclaimable(call_ref).await,
    };
    let (Enveloped { mut call, topology }, skew_offset_ms) = match read {
        Ok(hit) => hit,
        Err(miss) => return Materialised::Refused(refusal(origin, miss)),
    };
    reanchor(&mut call, skew_offset_ms);
    let now_ms = ctx.clock.now_ms();
    match origin {
        Origin::Takeover if call.state == CallModelState::Terminated => {
            ctx.metrics.bump_repl_takeover_refused_terminated();
            ctx.state.note_takeover(call_ref, "refused_terminated");
            return Materialised::Refused(Reason::Terminated);
        }
        Origin::Reclaim | Origin::Park
            if matches!(call.state, CallModelState::Terminated | CallModelState::Terminating) =>
        {
            let force_terminal = call.state == CallModelState::Terminating;
            let terminal = Enveloped::new(call, topology);
            discharge_materialized_terminal(ctx, call_ref, terminal, now_ms, force_terminal).await;
            return Materialised::Refused(Reason::Terminated);
        }
        _ => {}
    }
    let copy = Enveloped::new(call, topology);
    if !super::unrecoverable::copy_recoverable(ctx, call_ref, &copy, origin.into(), now_ms).await {
        return Materialised::Refused(Reason::Unrecoverable);
    }
    let Enveloped { mut call, topology } = copy;
    sanitize_restored_timers(
        &mut call.timers,
        call_ref,
        now_ms,
        ctx.config.keepalive_interval_sec * 1000,
        None,
    );
    crate::ring_deadline::arm_unrung_after_restore(&ctx.config, &mut call);
    // A copy acting on a call another node created mints its legs from a
    // space of its own; the creating node, reclaiming it, from the call's.
    call.leg_space = match origin {
        Origin::Takeover => call::LegSpace::Other,
        Origin::Reclaim | Origin::Park => call::LegSpace::Creator,
    };
    let pb = format_pb(topology.as_ref());
    if !ctx.state.materialize_if_absent(Enveloped::new(call.clone(), topology), origin.into()) {
        return Materialised::Resident(ctx.state.peek(call_ref).unwrap_or(call));
    }
    if let Origin::Park = origin {
        ctx.state.park(call_ref);
        ctx.metrics.bump_repl_reclaimed();
        return Materialised::Parked(call);
    }
    let seeded = serve(ctx, call_ref, &call, now_ms).await;
    match origin {
        Origin::Takeover => {
            ctx.state.mark_takeover(call_ref);
            let _ = ctx.txn.watch_self_release(call_ref).await;
            ctx.metrics.bump_repl_takeover_hydrated();
            ctx.state.note_takeover(call_ref, "hydrated");
            ctx.state.note_takeover_count(call_ref, "seeded", seeded.unwrap_or(0) as u64);
        }
        Origin::Reclaim => {
            ctx.metrics.bump_repl_reclaimed();
            // A lone reclaim is rare by construction and gets its own line
            // (ADR-0026); the bulk sweep folds into its single summary.
            tracing::info!(
                node = observe::node(),
                call_ref,
                call_id = %call.a_leg.call_id,
                pb = %pb,
                timers = call.timers.len(),
                "straggler reclaim"
            );
        }
        Origin::Park => {}
    }
    // The resident copy is the authoritative one: it carries this node's own
    // root span ids when the call is traced (ADR-0026 §5).
    Materialised::Served { call: ctx.state.peek(call_ref).unwrap_or(call), seeded }
}

/// Arm the resident `call` this node now serves: its ledger re-armed in the
/// timer service, its transactions seeded, a sent request past its deadline
/// settled as the timeout it drew. What the layer seeded, `None` when it could
/// not be asked.
async fn serve(ctx: &Arc<RouterCtx>, call_ref: &str, call: &Call, now_ms: i64) -> Option<usize> {
    ctx.timers.restore(call.timers.clone(), call_ref.to_string(), call.incarnation()).await;
    let seeded = ctx.txn.seed(call_ref, seeds::seeds_for(call, now_ms)).await.ok();
    for timeout in seeds::overdue_sent(call, now_ms) {
        let _ = ctx.reentry_tx.send(timeout);
    }
    seeded
}

/// Arm the parked call `call_ref` (ADR-0055 §8): its keepalives smoothed over
/// the bulk cohort when `smoothing` names one, then served as a fresh reclaim
/// is ([`serve`]). The caller holds the per-call lock. `None` when the call is
/// not parked: it is armed already, or gone.
pub(super) async fn arm(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    smoothing: Option<Smoothing>,
) -> Option<(Call, Option<usize>)> {
    if !ctx.state.unpark(call_ref) {
        return None;
    }
    let mut call = ctx.state.peek(call_ref)?;
    let now_ms = ctx.clock.now_ms();
    if smoothing.is_some() {
        sanitize_restored_timers(
            &mut call.timers,
            call_ref,
            now_ms,
            ctx.config.keepalive_interval_sec * 1000,
            smoothing,
        );
        ctx.state.update_quiet(call.clone());
    }
    let seeded = serve(ctx, call_ref, &call, now_ms).await;
    Some((call, seeded))
}

/// Re-offer `event` — a datagram the layer emitted with no transaction to
/// match it — against the transactions `call`'s materialisation seeded. A
/// response no client transaction matched and a CANCEL are always re-offered:
/// a seeded client INVITE ACKs the final it was waiting for, a seeded server
/// INVITE answers the CANCEL 200 + 487. An in-dialog INVITE is re-offered only
/// when the record names its branch as a relayed INVITE a peer already admitted
/// or an INVITE whose 2xx it retains ([`seeds::names_server_branch`]: a
/// retransmission the layer's own transaction absorbs, so the rules never mint
/// a second relay for the round nor refuse its copy as glare); a non-INVITE
/// request only when the record names its branch as a request it relayed or
/// holds ([`seeds::names_received_branch`]: a copy of it is absorbed, never
/// relayed a second time) — a relay whose client transaction this node does
/// not hold is superseded instead: dropped from the record, and the copy
/// relayed afresh; any other request, a response a transaction here already
/// took, and any non-SIP event are the turn's to process. `Matched`: the layer re-emitted
/// whatever it owes, so this turn ends without running the rules; `Unmatched`
/// (a layer that cannot be asked included): the turn continues.
pub(super) async fn reoffer_trigger(
    ctx: &Arc<RouterCtx>,
    call: Call,
    event: &CallEvent,
    matched_client_txn: bool,
) -> Offered {
    let CallEvent::Sip { message, src } = event else {
        return Offered::Turn(Box::new(call));
    };
    let offer = match message.as_ref() {
        SipMessage::Response(_) => !matched_client_txn,
        SipMessage::Request(req) if req.method() == Method::Cancel => true,
        SipMessage::Request(req) if req.method() == Method::Invite => {
            seeds::names_server_branch(&call, req.top_via().branch().unwrap_or_default())
        }
        SipMessage::Request(req) if req.method() == Method::Ack => false,
        SipMessage::Request(req) => {
            let branch = req.top_via().branch().unwrap_or_default();
            match seeds::received_record(&call, branch) {
                None => false,
                Some(record) => match record.branch() {
                    // A relay whose client transaction is another node's:
                    // its final follows that relay's Via there, so its
                    // originator's copy supersedes it and is relayed afresh.
                    Some(sent) if !ctx.txn.holds_client(sent).await.unwrap_or(false) => {
                        let call = seeds::drop_superseded(call, branch);
                        return Offered::Turn(Box::new(call));
                    }
                    _ => true,
                },
            }
        }
    };
    if !offer {
        return Offered::Turn(Box::new(call));
    }
    match ctx.txn.reoffer((**message).clone(), *src).await.unwrap_or(Reoffer::Unmatched) {
        Reoffer::Matched => Offered::Matched,
        Reoffer::Unmatched => Offered::Turn(Box::new(call)),
    }
}

/// What [`reoffer_trigger`] made of a turn's datagram.
pub(super) enum Offered {
    /// A transaction here took it: the turn ends.
    Matched,
    /// The turn runs on the call, a relay the datagram supersedes dropped.
    Turn(Box<Call>),
}

/// The seeds a meeting with the other copy hands the layer: the INVITE server
/// transactions answered 2xx whose ACK the met record still awaits, in RFC
/// 6026 Accepted with the 2xx it retains. They are the only transactions of
/// another node's rounds this node can serve while that node lives: a copy of
/// the INVITE reaches whichever node the proxy reads alive, and the 2xx that
/// answers it is in the record; the rest of a round runs where its Via sends
/// the target's responses, the node that relayed it.
pub(in crate::router) fn answered_invite_seeds(call: &Call) -> Vec<sip_txn::TxnSeed> {
    // No instant moves an INVITE seed: the latest one is read.
    seeds::seeds_for(call, i64::MAX)
        .into_iter()
        .filter(|s| matches!(s, sip_txn::TxnSeed::ServerAccepted { .. }))
        .collect()
}

/// Whether `call` names an INVITE transaction a live copy must serve — one a
/// materialisation would seed ([`seeds::seeds_for`]): an INVITE awaiting its
/// final, whose bound and finals the copy owns, or a 2xx awaiting its ACK,
/// whose retransmission ladder the copy resumes (RFC 3261 §13.3.1.4).
pub(super) fn has_invite_to_serve(call: &Call) -> bool {
    // No instant moves an INVITE seed: the latest one is read.
    seeds::seeds_for(call, i64::MAX).iter().any(|s| {
        matches!(
            s,
            sip_txn::TxnSeed::ClientInvite { .. }
                | sip_txn::TxnSeed::ServerInvite { .. }
                | sip_txn::TxnSeed::ServerAccepted { .. }
        )
    })
}

/// Whether `call` names a transaction a live copy must serve — one a
/// materialisation would seed ([`seeds::seeds_for`]): an INVITE awaiting its
/// final, whose bound and finals the copy owns, a 2xx awaiting its ACK, whose
/// retransmission ladder the copy resumes (RFC 3261 §13.3.1.4), or a
/// non-INVITE request record open on either side — a request sent awaiting
/// its final, whatever its deadline, one received awaiting its answer. A
/// sent request past its deadline is still work: the copy settles it as its
/// timeout ([`seeds::overdue_sent`]).
pub(super) fn has_work_to_serve(call: &Call) -> bool {
    // No deadline has passed at the earliest instant.
    !seeds::seeds_for(call, i64::MIN).is_empty()
}

/// The typed miss of the store read, named from the origin's side.
fn refusal(origin: Origin, miss: ReplicaMiss) -> Reason {
    match (origin, miss) {
        (Origin::Takeover, ReplicaMiss::WrongRole) => Reason::NotBackupRole,
        (Origin::Reclaim | Origin::Park, ReplicaMiss::WrongRole) => Reason::NotPrimaryRole,
        (_, ReplicaMiss::Absent) => Reason::NoReplica,
        (_, ReplicaMiss::Undecodable) => Reason::Decode,
    }
}
