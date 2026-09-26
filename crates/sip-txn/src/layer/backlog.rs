//! Admission against the deferred backlog: a new initial INVITE that would be
//! deferred behind a backlog already at its ceiling is refused statelessly,
//! before the 100 Trying and before any transaction exists, so the retry deque
//! stays bounded while the consumer is not draining the output queue.

use std::net::SocketAddr;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

use sip_message::emergency::is_emergency_request;
use sip_message::{SipRequest, SipResponse};
use sip_net::UdpEndpoint;

use super::owner::Owner;

/// Builds the response that refuses a new call at a ceiling. The consumer
/// phrases it (status, To-tag, `Retry-After`); the layer only sends it. No
/// transaction remembers the refusal, so it must answer every retransmission
/// of a request identically (RFC 3261 §8.2.7): its To-tag is derived from the
/// request.
pub type NewCallRefusal = Arc<dyn Fn(&SipRequest) -> SipResponse + Send + Sync>;

/// Ceilings on the deferred backlog (the critical events a full output queue
/// holds for later), applied to new initial INVITEs only. An INVITE carrying a
/// To-tag, a CANCEL, an ACK, a response and every event the layer emits for a
/// transaction it already holds belong to calls already admitted and are never
/// refused. A retransmission of an admitted INVITE matches its transaction
/// first and is never judged either.
#[derive(Clone)]
pub struct DeferredBound {
    /// Deferred events at which a new non-emergency initial INVITE is refused.
    pub normal: usize,
    /// Deferred events at which every new initial INVITE is refused, the
    /// emergency class (RFC 4412 `Resource-Priority`) included. Read as at
    /// least `normal`.
    pub emergency: usize,
    /// The response a refused INVITE draws.
    pub refusal: NewCallRefusal,
}

impl Owner {
    /// Refuse the new initial INVITE `req` when the output queue would defer
    /// it and the backlog already holds its class's ceiling: the refusal goes
    /// to `src` and is counted. `true` when refused — the caller creates no
    /// transaction and sends no 100 Trying.
    pub(super) async fn refuse_on_backlog(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        req: &SipRequest,
        src: SocketAddr,
    ) -> bool {
        let Some(bound) = &self.deferred_bound else { return false };
        let backlog = self.deferred_events.len();
        let would_defer = backlog > 0 || self.events_tx.capacity() == 0;
        if !would_defer || backlog < bound.normal {
            return false;
        }
        let emergency = is_emergency_request(req);
        if emergency && backlog < bound.emergency.max(bound.normal) {
            return false;
        }
        let refusal = (bound.refusal)(req);
        self.send_buffer(endpoint, refusal.image(), src).await;
        self.metrics.deferred_refused[usize::from(emergency)].fetch_add(1, Relaxed);
        true
    }
}
