//! [`Slots`] — the hard cap on one backend's requests in flight, and so on
//! its TCP connections. A request waits for a slot inside its own budget;
//! the wait never reaches the breaker. Saturation and wait time are counted
//! ([`super::metrics`]).

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

/// One backend's in-flight slots.
pub(super) struct Slots {
    semaphore: Arc<Semaphore>,
    peer: String,
}

impl Slots {
    /// `cap` slots for the backend `peer`.
    pub(super) fn new(cap: usize, peer: String) -> Self {
        Self { semaphore: Arc::new(Semaphore::new(cap.max(1))), peer }
    }

    /// A slot, waited for until `deadline`; `None` when none freed by then.
    pub(super) async fn take(&self, deadline: Instant) -> Option<OwnedSemaphorePermit> {
        if let Ok(permit) = self.semaphore.clone().try_acquire_owned() {
            return Some(permit);
        }
        super::metrics::slot_waited(&self.peer);
        let start = Instant::now();
        let permit = tokio::time::timeout_at(deadline, self.semaphore.clone().acquire_owned())
            .await
            .ok()
            .and_then(Result::ok)
            // A slot freed at the deadline leaves no time to send.
            .filter(|_| Instant::now() < deadline);
        super::metrics::slot_wait_ended(&self.peer, start.elapsed(), permit.is_some());
        permit
    }

    /// Slots free now.
    pub(super) fn free(&self) -> usize {
        self.semaphore.available_permits()
    }
}
