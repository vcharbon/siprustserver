//! The ACKs a leg holds back (a declared delayed automatic): parked until
//! due, sent on the confirmed dialog when due or before the leg's teardown
//! BYE, a refused send surfacing as the leg's transport failure.

use tokio::time::Instant;

use super::response::complete_originated_reinvite;
use super::runner::{ActorState, HeldAck};
use crate::agent::StepError;

/// Park until the earliest held ACK is due (or forever if none) — the delayed
/// automatic's own reactor arm.
pub(super) async fn wait_held_ack(held: &[HeldAck]) {
    match held.iter().map(|h| h.at).min() {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Fire the earliest held ACK: send it on the confirmed dialog (idempotent,
/// re-derivable — §13.2.2.4) and complete the bookkeeping the hold deferred.
pub(super) async fn fire_due_held_ack(st: &mut ActorState<'_>) -> Result<(), StepError> {
    let Some(pos) = st.held_acks.iter().enumerate().min_by_key(|(_, h)| h.at).map(|(i, _)| i)
    else {
        return Ok(());
    };
    let held = st.held_acks.swap_remove(pos);
    send_held_ack(st, held).await
}

/// Flush every held ACK NOW, out of schedule: a leg about to originate its
/// teardown BYE must not leave a 2xx un-ACKed behind it (RFC 3261 §15 — the
/// renegotiation completes before the dialog ends).
pub(super) async fn flush_held_acks(st: &mut ActorState<'_>) -> Result<(), StepError> {
    while let Some(held) = st.held_acks.pop() {
        send_held_ack(st, held).await?;
    }
    Ok(())
}

/// Send one held ACK and close its deferred bookkeeping.
async fn send_held_ack(st: &mut ActorState<'_>, held: HeldAck) -> Result<(), StepError> {
    if let Some(dialog) = st.dialogs.confirmed.as_mut() {
        dialog.try_ack_for(held.cseq, held.sdp.as_deref()).await?;
    }
    complete_originated_reinvite(st, held.cseq, Instant::now());
    Ok(())
}
