//! The refusal of a new INVITE on the identity of a call a peer discharged
//! because a poll of its turn never returned (ADR-0057 §6): the bare 503 at
//! router ingress, ahead of the admission ladder and of any routing consult,
//! so the routing answer that stuck the peer is never asked here. It covers
//! the identity's retries and the copies of its INVITE placed here as another
//! primary's, for [`DISCHARGED_WINDOW`](crate::repl::DISCHARGED_WINDOW) after
//! the discharge's delete applied
//! ([`DischargedIdentities`](crate::repl::DischargedIdentities)).
//!
//! The 503 carries no `Retry-After`: the previous hop sends this healthy node
//! no request for the duration one states (RFC 3261 §21.5.4), and only the
//! one identity is refused.

use std::net::SocketAddr;

use sip_message::{SipMessage, SipRequest};

use super::responses::build_unborn_503;
use super::{admit, RouterCtx};
use crate::admission::class_of;
use crate::new_calls::Refusal;
use b2bua_sdk::event::CallEvent;

/// Whether `req`, a new INVITE, is refused: initial (no To-tag) on the
/// Call-ID and From-tag of a call a peer discharged inside the window.
pub(super) fn refuses(ctx: &RouterCtx, req: &SipRequest) -> bool {
    req.to().tag().is_none()
        && ctx.state.repl_store().is_some_and(|repl| {
            let from_tag = req.from().tag().unwrap_or_default();
            repl.discharged().refuses(req.call_id().as_str(), from_tag)
        })
}

/// Answer `req`, a new INVITE from `src` that [`refuses`] refuses, the bare
/// 503 through its server transaction, and count it as a refused new call.
/// Its To-tag is derived from `req`, so every copy draws the same one; a
/// request whose lines no response can echo is left to its transaction's
/// lifetime.
pub(super) async fn refuse(ctx: &RouterCtx, req: &SipRequest, src: SocketAddr) {
    ctx.metrics.new_calls().reject(Refusal::Discharged, class_of(req));
    let minted = ctx.stateless_tagger.for_request(req).0;
    let advertised = &ctx.config.minted_final_advertisement;
    let Ok(resp) = build_unborn_503(req, minted, advertised) else { return };
    admit::send(&ctx.txn, resp, req, src).await;
}

/// Answer the initial INVITE `event` carries, another primary's copy, the
/// bare 503 through the server transaction the layer opened for it when its
/// identity is refused ([`refuses`]); whether it was answered. That primary
/// may have sent its 100 Trying: the caller gets her final from this live
/// worker (ADR-0022), not silence.
pub(super) async fn answers_copy(ctx: &RouterCtx, event: &CallEvent) -> bool {
    let CallEvent::Sip { message, src, .. } = event else { return false };
    let SipMessage::Request(req) = message.as_ref() else { return false };
    if !refuses(ctx, req) {
        return false;
    }
    refuse(ctx, req, *src).await;
    true
}
