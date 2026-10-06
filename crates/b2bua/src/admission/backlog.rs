//! The backlog rung's ceilings: the [`DeferredBound`] the transaction layer
//! judges a new INVITE with (ADR-0037 item 6), built here from the rung's row
//! and the one classifier ([`class_of`]).

use sip_txn::DeferredBound;

use super::ladder::class_of;

/// Deferred events at which a new normal call is refused, in output queues'
/// worth: one full queue waiting behind the full queue.
const NORMAL_QUEUES: usize = 1;
/// Deferred events at which an emergency call and an INVITE carrying a
/// To-tag are refused too, in output queues' worth.
const EMERGENCY_QUEUES: usize = 2;

/// The ceilings for an output queue of `event_capacity` events.
pub fn deferred_bound(event_capacity: usize) -> DeferredBound {
    ceilings(event_capacity * NORMAL_QUEUES, event_capacity * EMERGENCY_QUEUES)
}

/// The ceilings `normal` and `emergency`, in events.
pub fn ceilings(normal: usize, emergency: usize) -> DeferredBound {
    DeferredBound { normal, emergency, class_of }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ceilings_are_one_and_two_queues_of_backlog() {
        let bound = deferred_bound(4096);
        assert_eq!((bound.normal, bound.emergency), (4096, 8192));
    }
}
