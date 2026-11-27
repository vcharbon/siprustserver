//! The bytes of record bodies the pullers hand the router, bounded: a puller
//! holds a body's bytes of the budget before it hands the body up and waits
//! for them while the budget is spent, and the router gives them back when it
//! drops the body, its command handled. Nothing is dropped to keep the bound:
//! the stream the puller applies slows instead (ADR-0047 D7).

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The bytes of bodies on their way from the pullers to the router, at most:
/// four records at the default record cap.
pub const FLUSH_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// A byte budget shared by every puller of a node.
#[derive(Clone, Debug)]
pub struct FlushBudget {
    free: Arc<Semaphore>,
    bound: u32,
}

impl Default for FlushBudget {
    fn default() -> Self {
        Self::new(FLUSH_BUDGET_BYTES)
    }
}

impl FlushBudget {
    /// A budget of `bound` bytes, at least one and at most `u32::MAX`.
    pub fn new(bound: usize) -> Self {
        let bound = u32::try_from(bound).unwrap_or(u32::MAX).max(1);
        Self { free: Arc::new(Semaphore::new(bound as usize)), bound }
    }

    /// The budget's bound, in bytes.
    pub fn bound(&self) -> usize {
        self.bound as usize
    }

    /// The bytes held now.
    pub fn in_use(&self) -> usize {
        self.bound().saturating_sub(self.free.available_permits())
    }

    /// Make `body` hold its bytes of the budget, once they are free: a body
    /// larger than the budget holds all of it, an empty one nothing. Bytes it
    /// held before are given back first.
    pub(crate) async fn hold(&self, body: &mut FlushBody) {
        body.release();
        let share = u32::try_from(body.bytes.len()).unwrap_or(u32::MAX).min(self.bound);
        if share == 0 {
            return;
        }
        if let Ok(permit) = self.free.clone().acquire_many_owned(share).await {
            body.held = Some(Arc::new(permit));
        }
    }
}

/// A record body on its way from a puller to the router. While it holds
/// bytes of a [`FlushBudget`] they stay spent, and a clone holds the same
/// bytes: they come back when the last copy drops.
#[derive(Clone, Debug)]
pub struct FlushBody {
    bytes: Arc<[u8]>,
    held: Option<Arc<OwnedSemaphorePermit>>,
}

impl FlushBody {
    /// The body's bytes.
    pub fn bytes(&self) -> &Arc<[u8]> {
        &self.bytes
    }

    /// Give back the bytes of the budget this copy holds; the body stays.
    pub fn release(&mut self) {
        self.held = None;
    }

    /// Whether the body holds bytes of a budget.
    pub fn holds(&self) -> bool {
        self.held.is_some()
    }
}

impl From<Arc<[u8]>> for FlushBody {
    fn from(bytes: Arc<[u8]>) -> Self {
        Self { bytes, held: None }
    }
}

impl From<Vec<u8>> for FlushBody {
    fn from(bytes: Vec<u8>) -> Self {
        Self::from(Arc::<[u8]>::from(bytes))
    }
}

impl std::ops::Deref for FlushBody {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

#[cfg(test)]
mod tests;
