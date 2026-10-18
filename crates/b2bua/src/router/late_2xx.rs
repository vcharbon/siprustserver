//! The 2xx a released call still ACKs (RFC 3261 §13.2.2.4): a UAC core ACKs
//! every 2xx to its INVITE, and a callee repeats its 2xx until it hears that
//! ACK (§13.3.1.4). The client transaction ends at the first 2xx and the call
//! takes it, so a repeat reaching this node after the call was released —
//! its ACK lost, or its first copy taken by a turn that failed — finds
//! neither. It draws the bare ACK built from the 2xx alone, as an orphaned
//! transaction's does (ADR-0034), and no BYE: the call that took the first
//! copy ended that dialog itself.

use std::net::{IpAddr, SocketAddr};

use sip_message::generators::generate_ack_for_2xx_from_response;
use sip_message::{Method, SipRequest, SipResponse};

use super::RouterCtx;

/// ACK `resp` when it is a 2xx to an INVITE; whether it was. The ACK goes to
/// its first route's address, else back to `src`, the hop the 2xx came from.
pub(super) async fn ack(ctx: &RouterCtx, resp: &SipResponse, src: SocketAddr) -> bool {
    let is_invite_2xx =
        (200..300).contains(&resp.status()) && resp.cseq().method() == Method::Invite;
    if !is_invite_2xx {
        return false;
    }
    let Some(ack) = generate_ack_for_2xx_from_response(resp, &ctx.id_gen.new_branch()) else {
        return false;
    };
    let dest = first_route_address(&ack).unwrap_or(src);
    let _ = ctx.txn.send_raw(ack.image().to_vec(), dest).await;
    ctx.metrics.bump_unroutable_2xx_acked();
    true
}

/// The address the first `Route` of `req` names, where its host is one.
fn first_route_address(req: &SipRequest) -> Option<SocketAddr> {
    let routes = req.route_set().ok()?;
    let uri = routes.first()?.uri().clone();
    let host = uri.host().parse::<IpAddr>().ok()?;
    Some(SocketAddr::new(host, uri.port().unwrap_or(5060)))
}
