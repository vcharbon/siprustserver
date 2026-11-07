//! The CANCEL a caller sends while her initial INVITE's turn awaits its
//! routing decision (ADR-0055 §3). The `Cancelled` turn queues behind the
//! consult, so the INVITE's turn writes the CANCEL itself, as a turn point of
//! its own: the call as born, marked [`Call::setup_cancelled`]. The 200 to the
//! CANCEL and the 487 to the INVITE, which the transaction layer holds, leave
//! through the call's outbox once that write is applied, as any turn's
//! effects do. A node holding the marked call ends it with nothing on the
//! wire; the consult goes on, and its decision is recorded, never applied.

use std::future::Future;
use std::sync::Arc;

use call::Call;
use sip_message::SipRequest;

use super::commit::commit_cancel;
use super::RouterCtx;
use crate::initial_consult::Consulted;

/// Run `consult`, the consult of `call` whose caller's INVITE is `invite`,
/// writing the CANCEL that marks the setup while it runs.
pub(super) async fn consult_writing_its_cancel(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    call: &Call,
    invite: &SipRequest,
    consult: impl Future<Output = Consulted>,
) -> Consulted {
    let cseq = invite.cseq().seq();
    let watch = ctx.state.watch_setup_cancel(call_ref, cseq);
    let mut consult = std::pin::pin!(consult);
    tokio::select! {
        biased;
        consulted = &mut consult => return consulted,
        () = watch.marked() => {}
    }
    drop(watch);
    write_the_cancel(ctx, call_ref, call, invite).await;
    consult.await
}

/// Write `call` marked cancelled, with the release of the CANCEL's held
/// answers behind that write.
async fn write_the_cancel(ctx: &Arc<RouterCtx>, call_ref: &str, call: &Call, invite: &SipRequest) {
    let mut marked = call.clone();
    marked.setup_cancelled = true;
    commit_cancel(ctx, call_ref, marked, invite).await;
}
