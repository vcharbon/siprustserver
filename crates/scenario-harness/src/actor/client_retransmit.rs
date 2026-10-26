//! The leg's transaction layer over the unreliable transport, driven by its
//! own reactor arm: every request but ACK goes again until it is answered —
//! an INVITE on Timer A until any response, any other request on Timer E
//! until its final (RFC 3261 §17.1.1.2, §17.1.2.2) — and every final to an
//! INVITE until its ACK (§13.3.1.4, §17.2.1), each given up at 64·T1. The
//! ladders live with the endpoint ([`crate::absorption::Absorption`]); an
//! endpoint playing a peer that never retransmits opts out
//! ([`Agent::silence_retransmissions`](crate::Agent::silence_retransmissions)).

use tokio::time::Instant;

use super::runner::ActorState;
use crate::StepError;

impl ActorState<'_> {
    /// When the leg next owes a retransmission.
    pub(super) fn retransmit_due(&self) -> Option<Instant> {
        self.agent.next_due()
    }
}

/// Park until `at` (or forever if none).
pub(super) async fn wait(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Send again every datagram whose timer is due.
pub(super) async fn fire(st: &mut ActorState<'_>) -> Result<(), StepError> {
    st.agent.send_due().await;
    Ok(())
}
