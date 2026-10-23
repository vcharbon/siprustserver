//! A response to a request of ours whose call this node does not hold — no
//! live copy, no replica it may serve — answered as an unknown dialog would be
//! (ADR-0055 §4, [`b2bua_core::unheld_response`]): a 2xx is ACKed and BYEd, a
//! non-2xx INVITE final ACKed at the Request-URI its via-extension states, so
//! the peer stops retransmitting it (RFC 3261 §13.3.1.4, §17.2.1). A response
//! to a request this node sent is owed nothing here: its own transaction and
//! call absorbed the final and its repeats while they lived (§17.1.1.2).

use std::net::SocketAddr;

use sip_ids::TurnIds;
use sip_message::SipResponse;

use super::RouterCtx;
use crate::effects::{OutboundBody, OutboundTxnMode};

/// Send what `resp`, received from `src`, is owed with no call to hold it.
/// `ids` draws the branches.
pub(super) async fn answer(
    ctx: &RouterCtx,
    resp: &SipResponse,
    src: SocketAddr,
    ids: &mut TurnIds,
) {
    let local = (ctx.config.sip_local_ip.as_str(), ctx.config.sip_local_port);
    if resp.top_via().host_port() == local {
        return;
    }
    let from = (src.ip().to_string(), src.port());
    let now_ms = ctx.clock.now_ms();
    let answers =
        b2bua_core::unheld_response::answer_unheld_response(&ctx.config, resp, from, ids, now_ms);
    for eff in answers {
        let Some(dest) = sip_net::ip_socket_addr(&eff.destination.0, eff.destination.1) else {
            continue;
        };
        let OutboundBody::Request(req) = &eff.body else { continue };
        ctx.metrics.record_request_out(req.method().as_str());
        match eff.mode {
            OutboundTxnMode::NewClient(kind) => {
                let _ = ctx.txn.send_request(req.clone(), dest, kind).await;
            }
            _ => {
                let _ = ctx.txn.send_raw(req.image().to_vec(), dest).await;
            }
        }
    }
}
