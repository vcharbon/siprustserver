//! The router's rungs of the admission ladder (ADR-0037): a new INVITE is
//! judged at ingress — capacity, shed, panic-ELU, bucket, in
//! [`LADDER`](crate::admission::LADDER) order — before the dispatch offer, so
//! a refusal opens no per-call queue and waits for no handler permit. A
//! refused INVITE is answered through its server transaction, which absorbs
//! its retransmissions and the ACK, and is counted once. An admitted one
//! spends its CPS token when the offer queues its turn, and counts against
//! the live-call ceiling as an [`UnbornCall`] until its turn creates the
//! call. A copy of a call that is live or admitted — its INVITE merged back on
//! a new branch, matching its Call-ID, From-tag and CSeq (RFC 3261 §8.2.2.2) —
//! is not a new call and is not judged. An INVITE on the identity with
//! another CSeq is a new request (§8.1.3.5), judged as any other.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use call::Call;
use sip_message::SipRequest;
use sip_txn::TransactionLayer;

use super::RouterCtx;
use crate::admission::{class_of, first_refusal, Refusals, Refused, RouterReadings};
use crate::capacity::{CapacityReading, Occupancy};
use crate::dispatch::DispatchClass;

/// New calls admitted at ingress whose turn has not created their call yet,
/// by `callRef` and INVITE CSeq number. Clone shares them.
#[derive(Debug, Clone, Default)]
pub(crate) struct Unborn {
    inner: Arc<UnbornInner>,
}

#[derive(Debug, Default)]
struct UnbornInner {
    total: AtomicU64,
    by_call: Mutex<ByCall>,
}

/// The admitted INVITEs not born yet of each `callRef`: each one's CSeq
/// number and how many holds it has.
type ByCall = HashMap<Arc<str>, Vec<(u32, u32)>>;

impl Unborn {
    /// Admitted calls not born yet.
    pub(crate) fn count(&self) -> u64 {
        self.inner.total.load(Ordering::Acquire)
    }

    /// The live calls `live` reads, plus the admitted ones not born yet.
    /// The unborn count is read first: a birth between the two reads is then
    /// counted twice, never missed.
    pub(crate) fn with_live(&self, live: impl FnOnce() -> u64) -> u64 {
        let unborn = self.count();
        unborn + live()
    }

    /// Whether a new call on `call_ref` whose INVITE carries CSeq `cseq` is
    /// admitted and not born yet.
    pub(super) fn holds(&self, call_ref: &str, cseq: u32) -> bool {
        self.inner.lock().get(call_ref).is_some_and(|held| held.iter().any(|(c, _)| *c == cseq))
    }

    /// One more admitted call on `call_ref`, its INVITE carrying CSeq `cseq`,
    /// until the returned hold is dropped.
    pub(super) fn admit(&self, call_ref: &str, cseq: u32) -> UnbornCall {
        let mut by_call = self.inner.lock();
        let call_ref: Arc<str> =
            by_call.get_key_value(call_ref).map_or_else(|| Arc::from(call_ref), |(k, _)| k.clone());
        let held = by_call.entry(call_ref.clone()).or_default();
        match held.iter_mut().find(|(c, _)| *c == cseq) {
            Some((_, n)) => *n += 1,
            None => held.push((cseq, 1)),
        }
        drop(by_call);
        self.inner.total.fetch_add(1, Ordering::AcqRel);
        UnbornCall { inner: self.inner.clone(), call_ref, cseq }
    }
}

impl UnbornInner {
    fn lock(&self) -> std::sync::MutexGuard<'_, ByCall> {
        self.by_call.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// One new call admitted at ingress and not born yet. Its turn carries it and
/// drops it once the call is created; every other exit — a discard at the
/// offer, the store-fault 500, the identity guard's 500, a reaper gate, an
/// aborted or panicking body — drops it with the turn.
#[derive(Debug)]
pub(crate) struct UnbornCall {
    inner: Arc<UnbornInner>,
    call_ref: Arc<str>,
    cseq: u32,
}

impl Drop for UnbornCall {
    fn drop(&mut self) {
        let mut by_call = self.inner.lock();
        if let Some(held) = by_call.get_mut(&self.call_ref) {
            if let Some(at) = held.iter().position(|(c, _)| *c == self.cseq) {
                held[at].1 -= 1;
                if held[at].1 == 0 {
                    held.swap_remove(at);
                }
            }
            if held.is_empty() {
                by_call.remove(&self.call_ref);
            }
        }
        drop(by_call);
        self.inner.total.fetch_sub(1, Ordering::Release);
    }
}

/// Whether the new INVITE `req` of `call_ref` is a copy (RFC 3261 §8.2.2.2)
/// of a new call admitted and not born yet or of the live call: same
/// `callRef` and CSeq number. A copy is offered to that call unjudged. A
/// queue another request opened is no admitted call.
pub(super) fn is_copy(ctx: &RouterCtx, call_ref: &str, req: &SipRequest) -> bool {
    invite_here(ctx, call_ref, req.cseq().seq())
}

/// Whether the INVITE of `call_ref` carrying CSeq `cseq` is here: admitted
/// and not born yet, or the live call's own.
pub(super) fn invite_here(ctx: &RouterCtx, call_ref: &str, cseq: u32) -> bool {
    // The hold is read first: a turn creates its call before dropping its
    // hold, so a birth between the two reads is still seen.
    ctx.unborn.holds(call_ref, cseq)
        || ctx.state.peek(call_ref).is_some_and(|call| copies(&call, cseq))
}

/// Whether an INVITE carrying CSeq `cseq` on `call`'s identity is a copy of
/// `call`'s own: its INVITE carries the same CSeq. A copy is answered 482
/// even once the original's transaction has ended, which §8.2.2.2 does not
/// cover.
fn copies(call: &Call, cseq: u32) -> bool {
    call.a_leg_invite.cseq == cseq
}

/// Judge the new INVITE `req` of `call_ref` offered as `dispatch` on the
/// router rungs: refused, it is answered and counted here and `None` is
/// returned; admitted, the returned hold counts it until its call is born.
pub(super) async fn admit(
    ctx: &RouterCtx,
    call_ref: &str,
    req: &SipRequest,
    src: SocketAddr,
    dispatch: DispatchClass,
) -> Option<UnbornCall> {
    match judge(ctx, call_ref, req, dispatch) {
        Some(refused) => {
            answer(&ctx.txn, &ctx.refusals, req, src, refused).await;
            ctx.metrics.new_calls().reject(refused.reason, class_of(req));
            None
        }
        None => Some(ctx.unborn.admit(call_ref, req.cseq().seq())),
    }
}

/// The first router rung that refuses the new INVITE `req` of `call_ref`
/// offered as `dispatch`; `None` when every rung admits it.
fn judge(
    ctx: &RouterCtx,
    call_ref: &str,
    req: &SipRequest,
    dispatch: DispatchClass,
) -> Option<Refused> {
    first_refusal(class_of(req), &Readings { ctx, call_ref, dispatch })
}

/// The router's rung inputs for one new INVITE of `call_ref` offered as
/// `dispatch`.
struct Readings<'a> {
    ctx: &'a RouterCtx,
    call_ref: &'a str,
    dispatch: DispatchClass,
}

impl RouterReadings for Readings<'_> {
    fn capacity(&self) -> CapacityReading {
        self.ctx.capacity.reading(occupancy(self.ctx))
    }
    fn at_threshold(&self) -> bool {
        self.ctx.dispatcher.at_threshold(self.call_ref, self.dispatch)
    }
    fn panic_elu(&self) -> (f64, f64) {
        self.ctx.overload.panic_elu()
    }
    fn token_wait_sec(&self) -> u32 {
        self.ctx.overload.token_wait_sec()
    }
}

/// The live calls and transactions a new call is judged against: the calls
/// admitted and not born yet count as calls. Its own INVITE server
/// transaction is not one of them.
fn occupancy(ctx: &RouterCtx) -> Occupancy {
    Occupancy {
        calls: ctx.unborn.with_live(|| ctx.state.active_count() as u64),
        transactions: (ctx.txn.metrics().active_transactions() as u64).saturating_sub(1),
    }
}

/// The new INVITE's turn is queued: it spends a CPS token. The router's run
/// loop is the bucket's only taker, so a normal INVITE finds the token its
/// bucket rung saw; an emergency one spends one only when one is there.
pub(super) fn queued(ctx: &RouterCtx) {
    ctx.overload.spend_token();
}

/// Answer the new INVITE `req` from `src`, refused for `refused`, through its
/// server transaction. The caller counts it.
pub(super) async fn answer(
    txn: &TransactionLayer,
    refusals: &Refusals,
    req: &SipRequest,
    src: SocketAddr,
    refused: Refused,
) {
    let _ = txn.send_response(refusals.answer(req, refused), src).await;
}

#[cfg(test)]
mod tests;
