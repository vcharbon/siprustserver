//! The dispatch table written out as literals, independent of
//! [`DispatchClass::row`]: what the table test pins and what the queue
//! properties expect.

use super::class::{DispatchClass, Owed, PermitPool, QueueThreshold, Room};
use super::queue::Discard;
use crate::metrics::RemovalClass;

/// One expected row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Expected {
    pub room: Room,
    pub counted: bool,
    pub past_lifetime_cap: bool,
    pub unrun: Owed,
    pub ended: Owed,
    pub capped: Owed,
    pub pool: PermitPool,
    pub threshold: QueueThreshold,
}

/// The expected row of `class`.
pub(super) fn expected(class: DispatchClass) -> Expected {
    use DispatchClass as C;
    use Owed::*;
    use Room::*;
    let (room, counted, past_lifetime_cap, unrun, ended, capped) = match class {
        C::InitialInvite => (Bounded, true, false, NewCallRefused, NewCallRefused, NewCallRefused),
        C::EmergencyInvite => {
            (Bounded, true, false, NewCallRefused, NewCallRefused, NewCallRefused)
        }
        C::InDialogInvite => (Bounded, true, false, RetryLater, DialogGone, DialogGone),
        C::Ack => (Bounded, true, false, Nothing, Nothing, Nothing),
        C::OtherRequest => (Bounded, true, false, Forget, Forget, CappedRefusal),
        C::StrayCancel => (Bounded, true, false, Nothing, Nothing, CappedRefusal),
        C::Response => (Bounded, true, true, Nothing, Nothing, Nothing),
        C::TxnOutcome => (Always, true, true, Nothing, Nothing, Nothing),
        C::Cancelled => (PastBounds, true, true, Nothing, Nothing, Nothing),
        C::Timer => (Always, false, true, Nothing, Nothing, Nothing),
        C::Internal => (Always, false, true, Nothing, Nothing, Nothing),
    };
    let (pool, threshold) = match class {
        C::InitialInvite => (PermitPool::NewCall, QueueThreshold::NewCallHeadroom),
        _ => (PermitPool::Shared, QueueThreshold::FullCap),
    };
    Expected { room, counted, past_lifetime_cap, unrun, ended, capped, pool, threshold }
}

impl Expected {
    /// What a discard for `why` is expected to owe.
    pub fn owed(&self, why: Discard) -> Owed {
        match why {
            Discard::Capped => self.capped,
            Discard::Released(RemovalClass::Terminated) => self.ended,
            _ => self.unrun,
        }
    }

    /// Expected to keep its room on a capped call.
    pub fn admitted_when_capped(&self) -> bool {
        self.room == Room::Always || self.past_lifetime_cap
    }
}
