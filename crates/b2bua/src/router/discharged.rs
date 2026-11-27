//! The refusal of a new INVITE on the identity of a call a peer discharged
//! because a poll of its turn never returned (ADR-0057 §6): the bare 503 at
//! router ingress, ahead of the admission ladder and of any routing consult,
//! so the routing answer that stuck the peer is never asked here. It covers
//! the identity's retries and the copies of its INVITE placed here as another
//! primary's, for 64·T1 after the discharge's delete applied
//! ([`DischargedIdentities`](crate::repl::DischargedIdentities)).
//!
//! The 503 carries no `Retry-After`: the previous hop sends this healthy node
//! no request for the duration one states (RFC 3261 §21.5.4), and only the
//! one identity is refused.

use sip_message::generators::GenerateError;
use sip_message::{SipMessage, SipRequest, SipResponse};

use super::responses::build_unborn_503;
use super::{admit, RouterCtx};
use crate::admission::class_of;
use crate::new_calls::Refusal;
use b2bua_sdk::event::CallEvent;

/// The bare 503 refusing `req`, a new INVITE, when it is initial (no To-tag)
/// and a peer discharged a call on its Call-ID and From-tag in the last
/// 64·T1: its To-tag derived from `req`, so every copy draws the same one.
pub(super) fn refusal(
    ctx: &RouterCtx,
    req: &SipRequest,
) -> Option<Result<SipResponse, GenerateError>> {
    let refused = req.to().tag().is_none()
        && ctx.state.repl_store().is_some_and(|repl| {
            let from_tag = req.from().tag().unwrap_or_default();
            repl.discharged().refuses(req.call_id().as_str(), from_tag)
        });
    refused.then(|| {
        let minted = ctx.stateless_tagger.for_request(req).0;
        build_unborn_503(req, minted, &ctx.config.minted_final_advertisement)
    })
}

/// Answer the initial INVITE `event` carries, another primary's copy, the
/// bare 503 through the server transaction the layer opened for it when its
/// identity is refused ([`refusal`]), counted as a refused new call; whether
/// it was answered. That primary may have sent its 100 Trying: the caller
/// gets her final from this live worker (ADR-0022), not silence.
pub(super) async fn answers_copy(ctx: &RouterCtx, event: &CallEvent) -> bool {
    let CallEvent::Sip { message, src, .. } = event else { return false };
    let SipMessage::Request(req) = message.as_ref() else { return false };
    let Some(response) = refusal(ctx, req) else { return false };
    admit::send(&ctx.txn, response, req, *src).await;
    ctx.metrics.new_calls().reject(Refusal::Discharged, class_of(req));
    true
}
