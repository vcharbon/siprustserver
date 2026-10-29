//! A CANCEL the other owner's copy records on an INVITE whose client
//! transaction this node holds (ADR-0047 D7). The other owner decided it, but
//! its CANCEL could not leave there: a CANCEL goes on its INVITE's transaction
//! (RFC 3261 §9.1), and the node that sent the INVITE holds it. So when a
//! copy that records the CANCEL is met and taken here, this node sends it,
//! rebuilt from the INVITE's image as the record states it. The transaction
//! layer holds it until a provisional, or drops it once the INVITE has its
//! final.

use std::sync::Arc;

use call::{Call, Leg, TxnKind};
use sip_message::generators::{generate_cancel, InviteClientTransactionHandle};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipHeader, SipMessage, SipParser, SipRequest};

use super::super::RouterCtx;

/// Send each CANCEL `taken` records on a dialled INVITE that `live`, the
/// copy this node ran until the meeting, had not sent and the INVITE has no
/// final.
pub(super) async fn send_carried_cancels(ctx: &Arc<RouterCtx>, live: &Call, taken: &Call) {
    for leg in taken.b_legs.iter().filter(|leg| carried(live, leg)) {
        let Some((cancel, dest)) = rebuilt_cancel(leg) else { continue };
        let _ = ctx.txn.send_request(cancel, dest, TxnKind::Invite).await;
    }
}

/// Whether `leg`'s INVITE, still without a final, is CANCELled in the taken
/// copy and not in `live`.
fn carried(live: &Call, leg: &Leg) -> bool {
    let Some(invite) = leg.outbound_invite() else { return false };
    let sent_here = live
        .b_legs
        .iter()
        .find(|l| l.leg_id == leg.leg_id)
        .and_then(Leg::outbound_invite)
        .is_some_and(|i| i.cancel_sent);
    invite.cancel_sent && invite.final_received.is_none() && !sent_here
}

/// The CANCEL of `leg`'s dialled INVITE, stating the headers the record
/// keeps for it, and the INVITE's next hop, where the CANCEL goes too.
fn rebuilt_cancel(leg: &Leg) -> Option<(SipRequest, std::net::SocketAddr)> {
    let sent = leg.dialled_sent()?;
    let SipMessage::Request(original_invite) = CustomParser::new().parse(&sent.image).ok()? else {
        return None;
    };
    let stated: Vec<SipHeader> = leg
        .outbound_invite()?
        .cancel_headers
        .iter()
        .map(|(name, value)| SipHeader { name: name.clone().into(), value: value.clone().into() })
        .collect();
    let cancel = generate_cancel(&InviteClientTransactionHandle { original_invite }, &stated)
        .inspect_err(|e| tracing::warn!(leg_id = %leg.leg_id, error = %e, "a carried CANCEL makes no message"))
        .ok()?;
    let dest = sip_net::ip_socket_addr(&sent.destination.host, sent.destination.port)?;
    Some((cancel, dest))
}

#[cfg(test)]
mod tests;
