//! Arming a timer: the ledger's one arm ([`call::helpers::arm_timer`], which
//! replaces the key's instance under the call's next generation) and the
//! `ScheduleTimer` that hands the instance to the driver. Every arming site —
//! the rule executor, the route's own deadlines, the limiter refresh, an
//! off-turn request's answer deadline — goes through [`arm`].

use call::{Call, TimerKey};

use crate::effects::{CriticalStateEffect, HandlerEffects};

/// Arm `key` on `call` to fire at `fire_at` (epoch ms), replacing the
/// instance it held, and schedule it on `fx`.
pub fn arm(call: &mut Call, fx: &mut HandlerEffects, key: TimerKey, fire_at: i64) {
    let entry = call::helpers::arm_timer(call, key, fire_at);
    fx.critical.push(CriticalStateEffect::ScheduleTimer(entry));
}
