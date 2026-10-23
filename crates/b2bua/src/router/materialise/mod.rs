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
//!    ([`Materialised::Resident`]); its timers are live and never re-armed;
//! 2. the store read the origin names — `bak:{primary}` for a takeover,
//!    `pri:{self}` for a reclaim — each miss a typed [`Reason`] the caller's
//!    orphan path answers (481 for a request, drop for a response);
//! 3. the state check — a takeover refuses a `Terminated` replica (the image of
//!    a call that already ended, kept for its primary to fold or reclaim;
//!    serving it would re-arm timers and run rules on a released copy) and
//!    serves a `Terminating` one, which still owes the teardown's end; a
//!    reclaim discharges a terminal body through the reclaim funnel instead of
//!    re-serving a dead dialog, `Terminating` forced terminal (ADR-0020 X3);
//! 4. `reanchor` — every instant of the copy moved into this node's clock
//!    frame by the persisted skew offset (ADR-0055 §9), right after the read;
//!    then `sanitize_restored_timers` drops the stale `KeepaliveTimeout` and
//!    cohort-smooths on the bulk sweep only, and
//!    `ring_deadline::presume_after_restore` arms the ring deadline of an
//!    unanswered leg with no ring on record from its INVITE;
//! 5. the residency insert through the store, then `timers.restore`;
//! 6. **seed** — the in-flight INVITE transactions the record names, and the
//!    answered ones whose 2xx it retains ([`seeds::seeds_for`]), are rebuilt
//!    in the transaction layer, on every origin, so the layer owns their ACKs,
//!    their Timer D / G / L and the copies of their requests;
//! 7. takeover only: `mark_takeover` + `watch_self_release`, so the copy is
//!    shed once the transactions it serves — the seeds among them — clear
//!    (ADR-0014); the watch is armed after the seeds so it cannot fire on a
//!    copy that is about to hold one;
//! 8. metrics and the takeover episode line.
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

mod relayed_invite;
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
use crate::store::{MaterialiseOrigin, ReplicaMiss};
use b2bua_sdk::event::CallEvent;

/// Which entry path is materialising: the whole axis of behaviour (spec D5).
#[derive(Debug, Clone, Copy)]
pub(super) enum Origin {
    /// An acting-backup takeover of a crashed peer's call, read from
    /// `bak:{primary}`: the copy is marked and watched for self-release.
    Takeover,
    /// This node re-serving its own call from `pri:{self}`: the bulk sweep
    /// passes its cohort smoothing, a single straggler / on-demand reclaim
    /// passes `None`.
    Reclaim(Option<Smoothing>),
}

impl From<Origin> for MaterialiseOrigin {
    fn from(origin: Origin) -> Self {
        match origin {
            Origin::Takeover => MaterialiseOrigin::Takeover,
            Origin::Reclaim(_) => MaterialiseOrigin::Reclaim,
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
        return Materialised::Resident(live);
    }
    let read = match origin {
        Origin::Takeover => ctx.state.peek_replica(call_ref).await,
        Origin::Reclaim(_) => ctx.state.peek_reclaimable(call_ref).await,
    };
    let (mut call, skew_offset_ms) = match read {
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
        Origin::Reclaim(_)
            if matches!(call.state, CallModelState::Terminated | CallModelState::Terminating) =>
        {
            let force_terminal = call.state == CallModelState::Terminating;
            discharge_materialized_terminal(ctx, call_ref, call, now_ms, force_terminal).await;
            return Materialised::Refused(Reason::Terminated);
        }
        _ => {}
    }
    if !super::unrecoverable::copy_recoverable(ctx, call_ref, &call, origin.into(), now_ms).await {
        return Materialised::Refused(Reason::Unrecoverable);
    }
    let smoothing = match origin {
        Origin::Takeover => None,
        Origin::Reclaim(smoothing) => smoothing,
    };
    sanitize_restored_timers(
        &mut call.timers,
        call_ref,
        now_ms,
        ctx.config.keepalive_interval_sec * 1000,
        smoothing,
    );
    crate::ring_deadline::presume_after_restore(&ctx.config, &mut call);
    let timers = call.timers.clone();
    if !ctx.state.materialize_if_absent(call.clone(), origin.into()) {
        return Materialised::Resident(ctx.state.peek(call_ref).unwrap_or(call));
    }
    ctx.timers.restore(timers, call_ref.to_string(), call.incarnation()).await;
    let seeded = ctx.txn.seed(call_ref, seeds::seeds_for(&call)).await.ok();
    match origin {
        Origin::Takeover => {
            ctx.state.mark_takeover(call_ref);
            let _ = ctx.txn.watch_self_release(call_ref).await;
            ctx.metrics.bump_repl_takeover_hydrated();
            ctx.state.note_takeover(call_ref, "hydrated");
            ctx.state.note_takeover_count(call_ref, "seeded", seeded.unwrap_or(0) as u64);
        }
        Origin::Reclaim(smoothing) => {
            ctx.metrics.bump_repl_reclaimed();
            // The bulk sweep folds into `reclaim_all`'s single summary; a lone
            // reclaim is rare by construction and gets its own line (ADR-0026).
            if smoothing.is_none() {
                tracing::info!(
                    node = observe::node(),
                    call_ref,
                    call_id = %call.a_leg.call_id,
                    pb = %format_pb(&call),
                    timers = call.timers.len(),
                    "straggler reclaim"
                );
            }
        }
    }
    // The resident copy is the authoritative one: it carries this node's own
    // root span ids when the call is traced (ADR-0026 §5).
    Materialised::Served { call: ctx.state.peek(call_ref).unwrap_or(call), seeded }
}

/// Re-offer `event` — a datagram the layer emitted with no transaction to
/// match it — against the transactions `call`'s materialisation seeded. A
/// response no client transaction matched and a CANCEL are always re-offered:
/// a seeded client INVITE ACKs the final it was waiting for, a seeded server
/// INVITE answers the CANCEL 200 + 487. An in-dialog INVITE is re-offered only
/// when the record names its branch as a relayed INVITE a peer already admitted
/// or an INVITE whose 2xx it retains ([`seeds::names_server_branch`]: a
/// retransmission the layer's own transaction absorbs, so the rules never mint
/// a second relay for the round nor refuse its copy as glare);
/// any other request, a response a transaction here already took, and any
/// non-SIP event are the turn's to process. `Matched`: the layer re-emitted
/// whatever it owes, so this turn ends without running the rules; `Unmatched`
/// (a layer that cannot be asked included): the turn continues.
pub(super) async fn reoffer_trigger(
    ctx: &Arc<RouterCtx>,
    call: &Call,
    event: &CallEvent,
) -> Reoffer {
    let CallEvent::Sip { message, src, matched_client_txn } = event else {
        return Reoffer::Unmatched;
    };
    let offer = match message.as_ref() {
        SipMessage::Response(_) => !matched_client_txn,
        SipMessage::Request(req) if req.method() == Method::Cancel => true,
        SipMessage::Request(req) if req.method() == Method::Invite => {
            seeds::names_server_branch(call, req.top_via().branch().unwrap_or_default())
        }
        SipMessage::Request(_) => false,
    };
    if !offer {
        return Reoffer::Unmatched;
    }
    ctx.txn.reoffer((**message).clone(), *src).await.unwrap_or(Reoffer::Unmatched)
}

/// The seeds a fold into the call's record hands the layer: the INVITE server
/// transactions answered 2xx whose ACK the folded record still awaits, in RFC
/// 6026 Accepted with the 2xx it retains. They are the only transactions of
/// another node's rounds this node can serve while that node lives: a copy of
/// the INVITE reaches whichever node the proxy reads alive, and the 2xx that
/// answers it is in the record; the rest of a round runs where its Via sends
/// the target's responses, the node that relayed it.
pub(in crate::router) fn answered_invite_seeds(call: &Call) -> Vec<sip_txn::TxnSeed> {
    seeds::seeds_for(call)
        .into_iter()
        .filter(|s| matches!(s, sip_txn::TxnSeed::ServerAccepted { .. }))
        .collect()
}

/// Whether `call` names an INVITE transaction a live copy must serve — one a
/// materialisation would seed ([`seeds::seeds_for`]): an INVITE awaiting its
/// final, whose bound and finals the copy owns, or a 2xx awaiting its ACK,
/// whose retransmission ladder the copy resumes (RFC 3261 §13.3.1.4).
pub(super) fn has_invite_to_serve(call: &Call) -> bool {
    !seeds::seeds_for(call).is_empty()
}

/// The typed miss of the store read, named from the origin's side.
fn refusal(origin: Origin, miss: ReplicaMiss) -> Reason {
    match (origin, miss) {
        (Origin::Takeover, ReplicaMiss::WrongRole) => Reason::NotBackupRole,
        (Origin::Reclaim(_), ReplicaMiss::WrongRole) => Reason::NotPrimaryRole,
        (_, ReplicaMiss::Absent) => Reason::NoReplica,
        (_, ReplicaMiss::Undecodable) => Reason::Decode,
    }
}
