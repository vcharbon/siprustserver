//! The writers of the serializable timer ledger (`call.timers`): arm one key
//! under the call's next generation, disarm one, retire a selection. Timer
//! *types*, scopes, keys and entries live in [`crate::model::timer`]; live
//! scheduling lives in the b2bua timer driver.

use crate::model::{Call, TimerEntry, TimerKey};

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

/// Arm `key` to fire at `fire_at` under the call's next generation,
/// replacing the entry the key held; the entry to schedule.
pub fn arm_timer(call: &mut Call, key: TimerKey, fire_at: i64) -> TimerEntry {
    call.timer_generation += 1;
    let entry = TimerEntry { key, generation: call.timer_generation, fire_at };
    call.timers.retain(|t| t.key != entry.key);
    call.timers.push(entry.clone());
    entry
}

/// Take the entry `key` holds off the ledger, if any.
pub fn disarm_timer(call: &mut Call, key: &TimerKey) -> Option<TimerEntry> {
    let at = call.timers.iter().position(|t| &t.key == key)?;
    Some(call.timers.remove(at))
}

/// Take every entry `retired` selects off the ledger, in ledger order.
pub fn retire_timers(call: &mut Call, retired: impl Fn(&TimerEntry) -> bool) -> Vec<TimerEntry> {
    let (gone, kept) = std::mem::take(&mut call.timers).into_iter().partition(|t| retired(t));
    call.timers = kept;
    gone
}

/// Whether the ledger holds `key` armed under `generation`.
pub fn holds_timer(call: &Call, key: &TimerKey, generation: u64) -> bool {
    call.timers.iter().any(|t| &t.key == key && t.generation == generation)
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
