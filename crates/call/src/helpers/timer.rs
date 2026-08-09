//! Helpers over the serializable timer ledger (`call.timers`). Timer *types*
//! and entries live in [`crate::model::timer`]; live scheduling lives in the
//! b2bua timer driver.

use std::collections::BTreeMap;

use crate::model::TimerEntry;

/// Safety-net timer delay for the `terminating` state (ms). 32 s = SIP Timer
/// H/J — beyond it no legitimate BYE/2xx retransmit can land.
pub const TERMINATING_TIMEOUT_MS: i64 = 32_000;

/// The ledger's one-interval ceiling on a `Keepalive` deadline: `fire_at`
/// clamped down to `now_ms + keepalive_interval_ms`.
///
/// Every arming site re-arms the in-dialog probe at exactly the configured
/// cadence, so a deadline farther out is a foreign clock frame's residual, never
/// an intent — and it holds the call unprobed for a whole extra cadence, longer
/// than a peer's keepalive tolerance. Clamping moves a probe only earlier, which
/// costs nothing. `keepalive_interval_ms <= 0` (probing disabled) imposes no
/// ceiling. The single expression of the invariant: the rule-arming seam asserts
/// on it, the driver-only store-fault re-arm and restore-hygiene clamp through it
/// — every `Keepalive` deadline in the process passes one of the three.
pub fn cap_keepalive_fire_at(fire_at: i64, now_ms: i64, keepalive_interval_ms: i64) -> i64 {
    if keepalive_interval_ms <= 0 {
        return fire_at;
    }
    fire_at.min(now_ms + keepalive_interval_ms)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keepalive_deadline_is_capped_at_one_interval() {
        let now = 1_000_000;
        let interval = 300_000;
        assert_eq!(
            cap_keepalive_fire_at(now + interval + interval / 2, now, interval),
            now + interval,
            "a deadline beyond the cadence is clamped to one interval out",
        );
        assert_eq!(
            cap_keepalive_fire_at(now + interval / 3, now, interval),
            now + interval / 3,
            "a deadline inside the cadence is kept token-for-token",
        );
        assert_eq!(
            cap_keepalive_fire_at(now - interval, now, interval),
            now - interval,
            "a past-due deadline is left to the restore floor, not raised",
        );
        assert_eq!(
            cap_keepalive_fire_at(now + 10 * interval, now, 0),
            now + 10 * interval,
            "probing disabled (non-positive interval) imposes no ceiling",
        );
    }
}
