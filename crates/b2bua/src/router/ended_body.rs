//! What an ended body met with no live copy of its call here is owed
//! (ADR-0020 X3, ADR-0055 §3, §5): the final it may still owe its caller
//! ([`super::owed_final`]), the limiter release sent with no turn
//! ([`super::owed_limiter_key`]), and its discharge off the wire
//! ([`super::off_wire`]), forced when the body was caught mid-teardown. No
//! request the body awaits an answer to is sent again: a request the B2BUA
//! sends is made for its answer, and an ended or ending call needs none.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use call::{Call, CallModelState};

use super::off_wire;
use super::RouterCtx;
use crate::store::{Enveloped, MaterialiseOrigin};

/// Why this node met an ended body it holds no live copy of.
#[derive(Clone, Copy, Debug)]
pub(super) enum Cause {
    /// The call's primary departed: the departure sweep.
    Departed,
    /// The call's primary is read dead; `still` when the verdict still holds
    /// as the body is met.
    ReadDead { still: bool },
    /// This node's own call, reclaimed from its partition.
    Reclaim,
    /// An acting backup's reverse flush of this node's call.
    ReverseFlush,
    /// A call no node ended whose departed primary's replica outlived its TTL.
    Ghost,
    /// A deferred terminal no primary came back for, evicted at the reap.
    Evicted,
}

/// How the body is discharged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Discharge {
    /// As it is: it carries its own CDR.
    AsIs,
    /// Every leg forced terminal and a CDR appended.
    Forced,
}

/// What the body is owed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Disposition {
    pub(super) owed_final: bool,
    pub(super) limiter_release: bool,
    pub(super) discharge: Option<Discharge>,
}

/// What `body`, met for `cause`, is owed:
/// - a departure and a reclaim discharge it, forced when it is
///   `Terminating`, after its owed final: the node that wrote the body may
///   have died before the final left;
/// - a reverse flush does so for a `Terminated` body alone: the acting
///   backup still serves one that is ending;
/// - a dead verdict sends the owed final and, while the verdict holds, the
///   limiter release, and leaves the discharge to the departure;
/// - a ghost is discharged forced, with no final: no node ended it, so its
///   record holds none;
/// - an evicted deferred terminal is owed its limiter release alone: its TTL
///   outlived the caller's transaction, and no node is left to write its CDR.
pub(super) fn disposition(cause: Cause, body: &Call) -> Disposition {
    let as_found = match body.state {
        CallModelState::Terminating => Discharge::Forced,
        _ => Discharge::AsIs,
    };
    let (owed_final, limiter_release, discharge) = match cause {
        Cause::Departed | Cause::Reclaim => (true, false, Some(as_found)),
        Cause::ReverseFlush if body.state == CallModelState::Terminated => {
            (true, false, Some(Discharge::AsIs))
        }
        Cause::ReverseFlush => (false, false, None),
        Cause::ReadDead { still } => (true, still, None),
        Cause::Ghost => (false, false, Some(Discharge::Forced)),
        Cause::Evicted => (false, true, None),
    };
    Disposition { owed_final, limiter_release, discharge }
}

/// What [`pay`] did.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Paid {
    pub(super) discharged: bool,
    pub(super) released: bool,
}

/// Pay `body` of `call_ref` what it is owed for `cause` at `now_ms`
/// ([`disposition`]): the owed final first, then the limiter release, then
/// the discharge. A discharge needs the per-call lock held. Boxed: a body
/// met ended is rare, and the turns that may meet one stay small.
pub(super) fn pay<'a>(
    ctx: &'a Arc<RouterCtx>,
    call_ref: &'a str,
    cause: Cause,
    body: Enveloped,
    now_ms: i64,
) -> Pin<Box<dyn Future<Output = Paid> + Send + 'a>> {
    Box::pin(async move {
        let owed = disposition(cause, &body.call);
        if owed.owed_final {
            super::owed_final::deliver(ctx, call_ref, &body.call).await;
        }
        let released = owed.limiter_release && super::owed_limiter_key::release(ctx, &body.call);
        let discharge = owed.discharge.and_then(|how| discharge(ctx, call_ref, body, now_ms, how));
        let discharged = discharge.is_some();
        if let Some(commit) = discharge {
            commit.await;
        }
        Paid { discharged, released }
    })
}

/// The discharge of `body` off the wire ([`off_wire::Ending::Reached`]),
/// materialised first so `backup_of` resolves and the propagated delete
/// reaches the peer; `None` when the call is already resident.
fn discharge<'a>(
    ctx: &'a Arc<RouterCtx>,
    call_ref: &'a str,
    body: Enveloped,
    now_ms: i64,
    how: Discharge,
) -> Option<impl Future<Output = ()> + Send + 'a> {
    if !ctx.state.materialize_if_absent(body.clone(), MaterialiseOrigin::Reclaim) {
        return None;
    }
    let ending = off_wire::Ending::Reached { body: body.call, forced: how == Discharge::Forced };
    let label = &"the discharge of a body met ended";
    Some(off_wire::end(ctx, call_ref, ending, off_wire::Caller::Leave, now_ms, label))
}

#[cfg(test)]
mod tests;
