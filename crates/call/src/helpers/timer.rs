//! Helpers over the serializable timer ledger (`call.timers`). Timer *types*
//! and entries live in [`crate::model::timer`]; live scheduling lives in the
//! b2bua timer driver.

use std::collections::BTreeMap;

use crate::model::TimerEntry;

/// Safety-net timer delay for the `terminating` state (ms). 32 s = SIP Timer
/// H/J — beyond it no legitimate BYE/2xx retransmit can land.
pub const TERMINATING_TIMEOUT_MS: i64 = 32_000;

/// Replace any existing entry with the same id, then append the new one. Panics
/// in debug builds if two entries end up sharing an id (an upstream caller
/// bypassed this helper).
pub fn replace_timer_by_id(existing: Vec<TimerEntry>, entry: TimerEntry) -> Vec<TimerEntry> {
    let mut next: Vec<TimerEntry> = existing.into_iter().filter(|t| t.id != entry.id).collect();
    next.push(entry);
    debug_assert!(
        {
            let mut seen = BTreeMap::new();
            next.iter().all(|t| seen.insert(t.id.clone(), ()).is_none())
        },
        "replace_timer_by_id invariant violated: duplicate timer id"
    );
    next
}
