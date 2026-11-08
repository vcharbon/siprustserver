//! The turn of a copy of the caller's initial INVITE of a call whose bulk park
//! was deferred ([`super::deferred_parks`]). The copy is judged at its turn,
//! as a parked call's copy is at that moment, and never makes the call
//! resident or arms it: a call still parked answers it as any parked call
//! does (`initial_turn`, merged); a call armed since holds the INVITE's
//! rebuilt transaction, which absorbs the copy; a call gone by then never
//! answered its caller, who gets the bare 503 on the transaction the copy
//! kept.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use sip_message::{SipMessage, SipRequest};

use super::unroutable::answer_gone_copy;
use super::RouterCtx;
use crate::turn_scope::TurnScope;
use b2bua_sdk::event::CallEvent;

/// What became of a deferred call's copy at its turn.
pub(super) enum Judged {
    /// The call is still parked: the copy is answered as a parked call's.
    Parked,
    /// The call is armed: its INVITE's transaction took the copy.
    Absorbed,
    /// The call is gone: the copy drew the bare 503, and nothing of the call
    /// is left here but this turn's queue.
    Gone,
}

/// Judge the copy `req` from `src`, which `event` carries, of `call_ref`'s
/// initial INVITE, the copy's identifiers drawn from `turn`. Boxed: a copy of
/// a deferred call is rare, and the turn body that runs every event stays
/// small.
pub(super) fn judge<'a>(
    ctx: &'a Arc<RouterCtx>,
    call_ref: &'a str,
    req: &'a SipRequest,
    src: SocketAddr,
    event: &'a CallEvent,
    turn: &'a mut TurnScope,
) -> Pin<Box<dyn Future<Output = Judged> + Send + 'a>> {
    Box::pin(async move {
        if !ctx.state.contains(call_ref) {
            answer_gone_copy(ctx, event, turn.ids.new_tag()).await;
            ctx.metrics.new_calls().refuse_copy();
            return Judged::Gone;
        }
        if ctx.state.is_parked(call_ref) {
            return Judged::Parked;
        }
        let _ = ctx.txn.reoffer(SipMessage::Request(req.clone()), src).await;
        Judged::Absorbed
    })
}
