//! The messages a turn holds for its backup (ADR-0055 §3): the ACK of a 2xx and
//! the first provisional of an INVITE, the messages that stop a peer
//! retransmitting. Each leaves once the call's backup applied the call's
//! current change-log entry, or once the cap expires, whichever comes first;
//! a call with no live, bootstrapped backup holds nothing. A held ACK leaves
//! before any later message of its call (RFC 3261 §14.1). Every other message
//! leaves at once, and no turn waits on a hold.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use call::TxnKind;
use sip_message::SipRequest;

use super::RouterCtx;
use crate::poison::{locked, HELD_ACKS};

/// An ACK waiting for its call's backup: sent once, by whichever comes first
/// of its hold's end and a later message of its call. The take and the send
/// are one step under the slot's lock, so a later message that finds the slot
/// empty leaves after the ACK, whatever thread sent it.
#[derive(Default)]
struct HeldAck(tokio::sync::Mutex<Option<(SipRequest, SocketAddr)>>);

impl HeldAck {
    /// Send the ACK unless it already left.
    async fn send(&self, ctx: &RouterCtx) {
        let mut slot = self.0.lock().await;
        if let Some((ack, dest)) = slot.take() {
            send_ack(ctx, &ack, dest).await;
        }
    }
}

/// The ACKs each call holds, and the cap of every hold.
pub struct Holds {
    cap: Duration,
    acks: Mutex<HashMap<String, Vec<Arc<HeldAck>>>>,
}

impl Holds {
    /// Holds lasting `cap` at most.
    pub fn new(cap: Duration) -> Self {
        Self { cap, acks: Mutex::default() }
    }

    fn acks(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<Arc<HeldAck>>>> {
        locked(&self.acks, HELD_ACKS)
    }

    /// Forget `held` of `call_ref`, sent.
    fn forget(&self, call_ref: &str, held: &Arc<HeldAck>) {
        let mut acks = self.acks();
        if let Some(list) = acks.get_mut(call_ref) {
            list.retain(|h| !Arc::ptr_eq(h, held));
            if list.is_empty() {
                acks.remove(call_ref);
            }
        }
    }

    /// The ACKs `call_ref` holds now, in the order its turns held them.
    fn take_all(&self, call_ref: &str) -> Vec<Arc<HeldAck>> {
        self.acks().remove(call_ref).unwrap_or_default()
    }
}

/// Send the ACK `ack` of `call_ref` toward `dest`: held for the call's backup,
/// or at once when the call holds nothing.
pub(super) async fn ack(ctx: &Arc<RouterCtx>, call_ref: &str, ack: &SipRequest, dest: SocketAddr) {
    if ctx.state.hold_target(call_ref).is_none() {
        send_ack(ctx, ack, dest).await;
        return;
    }
    let held = Arc::new(HeldAck(tokio::sync::Mutex::new(Some((ack.clone(), dest)))));
    ctx.holds.acks().entry(call_ref.to_string()).or_default().push(held.clone());
    let (ctx, call_ref) = (ctx.clone(), call_ref.to_string());
    observe::spawn_keeping_node(async move {
        let _ = tokio::time::timeout(ctx.holds.cap, ctx.state.applied_current(&call_ref)).await;
        held.send(&ctx).await;
        ctx.holds.forget(&call_ref, &held);
    });
}

/// Send now every ACK `call_ref`'s earlier turns still hold: a later message
/// of the call is about to leave (RFC 3261 §14.1).
pub(super) async fn release(ctx: &RouterCtx, call_ref: &str) {
    for held in ctx.holds.take_all(call_ref) {
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
