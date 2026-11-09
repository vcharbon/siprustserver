//! What the router sends behind a call's latest write outside a handler
//! turn's commit: the caller's 100 after the creation write, the answer to a
//! request it refused, the server transactions another copy answered. A
//! rider reports no turn mark.

use std::future::Future;
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
    /// What sending it does once the outbox releases it, sized to its kind.
    fn into_effects(self, ctx: Arc<RouterCtx>) -> Effects {
        match self {
            Rider::Trying(invite) => Box::new(move |_| {
                Box::pin(async move {
                    let _ = ctx.txn.release_trying(&invite).await;
                })
            }),
            Rider::Answer(answer, dest) => Box::new(move |_| {
                Box::pin(async move {
                    let _ = ctx.txn.send_response(*answer, dest).await;
                })
            }),
            Rider::Relinquish(keys) => Box::new(move |_| {
                Box::pin(async move {
                    for key in keys {
                        let _ = ctx.txn.relinquish_server(key).await;
                    }
                })
            }),
        }
    }
}

/// Queue `rider` on `call_ref`'s outbox behind the call's latest write, read
/// as this is called; the returned push is awaited at once.
pub fn behind_the_write<'a>(
    ctx: &'a Arc<RouterCtx>,
    call_ref: &'a str,
    rider: Rider,
) -> impl Future<Output = ()> + 'a {
    let wait = ctx.state.hold_target(call_ref);
    ctx.outboxes.push(call_ref, wait, rider.into_effects(ctx.clone()))
}
