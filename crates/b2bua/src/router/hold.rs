//! The messages a turn holds for its backup (ADR-0055 §3): the ACK of a 2xx,
//! the first provisional of an INVITE and the final to a non-INVITE request,
//! the messages that stop a peer retransmitting. Each leaves once the call's
//! backup applied the call's current change-log entry, or once the cap
//! expires, whichever comes first; a call with no live, bootstrapped backup
//! holds nothing. A held ACK or final leaves before any later message of its
//! dialog (RFC 3261 §14.1). Every other message leaves at once, and no turn
//! waits on a hold.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use call::TxnKind;
use sip_message::{SipRequest, SipResponse};

use super::RouterCtx;
use crate::poison::{locked, HELD_ACKS};

/// A message a hold keeps back: the ACK of a 2xx, or the final to a
/// non-INVITE request.
enum Kept {
    Ack(Box<SipRequest>),
    Final(Box<SipResponse>),
}

/// A message waiting for its call's backup: sent once, by whichever comes
/// first of its hold's end and a later message of its call. The take and the
/// send are one step under the slot's lock, so a later message that finds the
/// slot empty leaves after it, whatever thread sent it.
struct Held {
    /// The Call-ID of the dialog the message rides.
    call_id: String,
    slot: tokio::sync::Mutex<Option<(Kept, SocketAddr)>>,
}

impl Held {
    /// Send the message unless it already left.
    async fn send(&self, ctx: &RouterCtx) {
        let mut slot = self.slot.lock().await;
        match slot.take() {
            Some((Kept::Ack(ack), dest)) => send_ack(ctx, &ack, dest).await,
            Some((Kept::Final(resp), dest)) => {
                let _ = ctx.txn.send_response(*resp, dest).await;
            }
            None => {}
        }
    }
}

/// The messages each call holds, and the cap of every hold.
pub struct Holds {
    cap: Duration,
    held: Mutex<HashMap<String, Vec<Arc<Held>>>>,
}

impl Holds {
    /// Holds lasting `cap` at most.
    pub fn new(cap: Duration) -> Self {
        Self { cap, held: Mutex::default() }
    }

    fn held(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<Arc<Held>>>> {
        locked(&self.held, HELD_ACKS)
    }

    /// Forget `held` of `call_ref`, sent.
    fn forget(&self, call_ref: &str, held: &Arc<Held>) {
        let mut acks = self.held();
        if let Some(list) = acks.get_mut(call_ref) {
            list.retain(|h| !Arc::ptr_eq(h, held));
            if list.is_empty() {
                acks.remove(call_ref);
            }
        }
    }

    /// The messages `call_ref` holds now, in the order its turns held them.
    fn take_all(&self, call_ref: &str) -> Vec<Arc<Held>> {
        self.held().remove(call_ref).unwrap_or_default()
    }

    /// The messages `call_ref` holds now on the dialogs of `call_id`, in the
    /// order its turns held them.
    fn take_dialog(&self, call_ref: &str, call_id: &str) -> Vec<Arc<Held>> {
        let mut held = self.held();
        let Some(list) = held.get_mut(call_ref) else { return Vec::new() };
        let (taken, kept) = std::mem::take(list).into_iter().partition(|h| h.call_id == call_id);
        *list = kept;
        if list.is_empty() {
            held.remove(call_ref);
        }
        taken
    }
}

/// Send the ACK `ack` of `call_ref` toward `dest`: held for the call's backup,
/// or at once when the call holds nothing or the turn `ends` it, the call
/// released with it (ADR-0055 §3).
pub(super) async fn ack(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    ack: &SipRequest,
    dest: SocketAddr,
    ends: bool,
) {
    if ends || ctx.state.hold_target(call_ref).is_none() {
        send_ack(ctx, ack, dest).await;
        return;
    }
    hold(ctx, call_ref, &ack.call_id().to_string(), Kept::Ack(Box::new(ack.clone())), dest);
}

/// Send `resp`, the final to a non-INVITE request of `call_ref`, toward
/// `dest`: held for the call's backup, or at once when the call holds nothing
/// or the turn `ends` it, the call released with it (ADR-0055 §3).
pub(super) async fn non_invite_final(
    ctx: &Arc<RouterCtx>,
    call_ref: &str,
    resp: &SipResponse,
    dest: SocketAddr,
    ends: bool,
) {
    if ends || ctx.state.hold_target(call_ref).is_none() {
        let _ = ctx.txn.send_response(resp.clone(), dest).await;
        return;
    }
    hold(ctx, call_ref, &resp.call_id().to_string(), Kept::Final(Box::new(resp.clone())), dest);
}

/// Keep `kept` of `call_ref` until the call's backup applied its current
/// change-log entry, or the cap.
fn hold(ctx: &Arc<RouterCtx>, call_ref: &str, call_id: &str, kept: Kept, dest: SocketAddr) {
    let slot = tokio::sync::Mutex::new(Some((kept, dest)));
    let held = Arc::new(Held { call_id: call_id.to_string(), slot });
    ctx.holds.held().entry(call_ref.to_string()).or_default().push(held.clone());
    let (ctx, call_ref) = (ctx.clone(), call_ref.to_string());
    observe::spawn_keeping_node(async move {
        let _ = tokio::time::timeout(ctx.holds.cap, ctx.state.applied_current(&call_ref)).await;
        held.send(&ctx).await;
        ctx.holds.forget(&call_ref, &held);
    });
}

/// Send now every message `call_ref`'s earlier turns still hold: the call
/// is released.
pub(super) async fn release(ctx: &RouterCtx, call_ref: &str) {
    for held in ctx.holds.take_all(call_ref) {
        held.send(ctx).await;
    }
}

/// Send now every message `call_ref`'s earlier turns still hold on the
/// dialogs of `call_id`: a later message of that dialog is about to leave
/// (RFC 3261 §14.1).
pub(super) async fn release_dialog(ctx: &RouterCtx, call_ref: &str, call_id: &str) {
    for held in ctx.holds.take_dialog(call_ref, call_id) {
        held.send(ctx).await;
    }
}

/// Release the 100 Trying of the INVITE `invite` of `call_ref` once the call's
/// backup applied the call's current change-log entry, or at the cap; at once
/// when the call holds nothing. The transaction layer sends it only while the
/// INVITE has drawn no response, and itself at the §17.2.1 bound.
pub(super) async fn first_provisional(ctx: &Arc<RouterCtx>, call_ref: &str, invite: &SipRequest) {
    if ctx.state.hold_target(call_ref).is_none() {
        let _ = ctx.txn.release_trying(invite).await;
        return;
    }
    let (ctx, invite, call_ref) = (ctx.clone(), invite.clone(), call_ref.to_string());
    observe::spawn_keeping_node(async move {
        let _ = tokio::time::timeout(ctx.holds.cap, ctx.state.applied_current(&call_ref)).await;
        let _ = ctx.txn.release_trying(&invite).await;
    });
}

/// The ACK through its INVITE client transaction, raw: the Accepted
/// transaction of its 2xx keeps it for the 2xx's repeats (RFC 6026 §7.2).
async fn send_ack(ctx: &RouterCtx, ack: &SipRequest, dest: SocketAddr) {
    let _ = ctx.txn.send_request(ack.clone(), dest, TxnKind::Invite).await;
}
