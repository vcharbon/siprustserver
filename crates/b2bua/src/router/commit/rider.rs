//! What the router sends behind a call's latest write for a turn that lands
//! no record of its own: the caller's 100 after the creation write, the
//! answer to a request it refused, the server transactions another copy
//! answered. A rider reports no turn mark.

use std::net::SocketAddr;
use std::sync::Arc;

use sip_message::{SipRequest, SipResponse};
use sip_txn::ServerTxnKey;

use crate::router::outbox::Effects;
use crate::router::RouterCtx;

/// One thing the router sends behind a call's latest write.
pub enum Rider {
    /// The caller's 100 to the INVITE whose call the write created.
    Trying(Box<SipRequest>),
    /// The router's answer to a request it refused, to where it is sent.
    Answer(Box<SipResponse>, SocketAddr),
    /// The server transactions of requests another copy answered.
    Relinquish(Vec<ServerTxnKey>),
}

impl Rider {
    async fn send(self, ctx: &RouterCtx) {
        match self {
            Rider::Trying(invite) => {
                let _ = ctx.txn.release_trying(&invite).await;
            }
            Rider::Answer(answer, dest) => {
                let _ = ctx.txn.send_response(*answer, dest).await;
            }
            Rider::Relinquish(keys) => {
                for key in keys {
                    let _ = ctx.txn.relinquish_server(key).await;
                }
            }
        }
    }
}

/// Queue `rider` on `call_ref`'s outbox behind the call's latest write.
pub async fn behind_the_write(ctx: &Arc<RouterCtx>, call_ref: &str, rider: Rider) {
    let sender = ctx.clone();
    behind(ctx, call_ref, Box::new(move |_| Box::pin(async move { rider.send(&sender).await })))
        .await;
}

/// Queue `effects` on `call_ref`'s outbox, waiting for the backup to apply
/// the call's current change-log entry.
pub(super) async fn behind(ctx: &Arc<RouterCtx>, call_ref: &str, effects: Effects) {
    let wait = ctx.state.hold_target(call_ref);
    ctx.outboxes.push(call_ref, wait, effects).await;
}
