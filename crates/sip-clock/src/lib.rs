//! sip-clock — the clock seam (slice 0 of the migration; re-expression of the
//! source's Effect `Clock` / `TestClock`).
//!
//! # Why this exists (and why it is *narrow*)
//!
//! Effect's `Clock`/`TestClock` did two jobs through one runtime-injected seam:
//! it answered "what time is it?" (the `nowMs` value flowing into deadline math)
//! **and** "wake me later" (scheduling). In Rust those split:
//!
//! - **Behaviour — timers, deadlines, idle-sweeps, windows — runs on monotonic
//!   time via `tokio::time` directly** (`sleep`, `sleep_until`, `interval`,
//!   `timeout`, `Instant`). `tokio::time::pause`/`advance` is the universal test
//!   lever for all of it, ambient within the runtime exactly as Effect's
//!   `TestClock` was ambient within the Effect runtime. There is **no trait
//!   wrapping** of scheduling — wrapping it would re-implement a worse tokio.
//! - **Wall-clock `now_ms()` is for timestamps only** — log lines, call records.
//!   It is *not* a behavioural input (no deadline is computed from it). It is the
//!   one thing `tokio::time::pause` cannot bend (pause moves the monotonic clock,
//!   not `SystemTime`), so it gets the injectable seam here.
//!
//! # The construction
//!
//! [`Clock::now_ms`] is **monotonic-anchored**: `anchor_wall_ms +
//! elapsed_since_anchor`, where the elapsed is measured against
//! `tokio::time::Instant`. Two consequences fall out for free:
//!
//! - In prod the timestamp never jumps backward (it rides the monotonic clock),
//!   at the cost of drifting from true wall time over long uptime — fine for
//!   logs/records; read [`std::time::SystemTime`] directly at the rare call site
//!   that must reconcile with an external wall clock (a SIP `Date` header, a
//!   cross-system billing record).
//! - In tests the elapsed rides the **same** monotonic clock `tokio::time`
//!   controls, so a single `tokio::time::advance(d)` moves the behavioural timers
//!   **and** `now_ms()` together, consistently. No separately-settable
//!   `TestClock` counter is needed — pause/advance is the one lever.
//!
//! # HA note — failover timer reconstruction (landed)
//!
//! Monotonic `Instant`s are not portable across processes / restarts / replicas,
//! so a replicated timer can never ship a raw `Instant`. The failover slice chose
//! the **absolute-wall-deadline** option: each `call::TimerEntry` carries
//! `fire_at` as an epoch-ms deadline (`now_ms()` at schedule time + the delay),
//! which IS replicated as part of the `Call`. On takeover the standby rebuilds its
//! local monotonic timer from that deadline — see
//! `b2bua::timers::TimerService::restore`.
//!
//! Consequence — **this is the one place `now_ms()` is a *behavioural*,
//! cross-node input.** Everywhere else it is timestamps only; and even the timer
//! driver's `fire_at - now_ms()` is *not* load-bearing within a single process,
//! because `fire_at` was minted from that same `now_ms()` and the two readings
//! cancel (the delay reduces to the original `delay_ms`). Across a failover they
//! do not cancel: a `fire_at` minted on the dead node is compared against the live
//! node's `now_ms()`, so the rearmed deadline is only as accurate as the two
//! nodes' **wall clocks agree** (keep them NTP-disciplined), plus each node's
//! monotonic drift from true wall over its uptime. Skew shifts the rearmed
//! deadline earlier/later by the disagreement; a past-due `fire_at` clamps to a
//! zero delay and fires immediately. See docs/MIGRATION_PLAN_B2B.md §2.

use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::Instant;

/// A monotonic-anchored clock. Cheap to [`Clone`] (two scalars); share one
/// instance everywhere that needs a timestamp so all readers sit on the same
/// timeline.
///
/// Behavioural code does **not** take a `Clock` — it calls `tokio::time`
/// directly. `Clock` is injected only into code that *timestamps* (logs, call
/// records), which is also what makes those timestamps deterministic in tests.
#[derive(Clone, Debug)]
pub struct Clock {
    anchor_wall_ms: i64,
    anchor_instant: Instant,
}

impl Clock {
    /// Production constructor: anchor to the real wall clock once, now.
    ///
    /// Subsequent [`now_ms`](Clock::now_ms) reads add the monotonic elapsed to
    /// this anchor, so the returned timestamp is wall-clock-aligned at startup
    /// and monotonic (never decreasing) thereafter.
    pub fn system() -> Self {
        let anchor_wall_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self {
            anchor_wall_ms,
            anchor_instant: Instant::now(),
        }
    }

    /// Test constructor: pin the wall anchor to a fixed epoch-ms value.
    ///
    /// Under a paused runtime (`#[tokio::test(start_paused = true)]` or
    /// `tokio::time::pause()`), `now_ms()` then advances in lockstep with
    /// `tokio::time::advance`, giving fully deterministic timestamps.
    pub fn test_at(anchor_wall_ms: i64) -> Self {
        Self {
            anchor_wall_ms,
            anchor_instant: Instant::now(),
        }
    }

    /// Wall-ish timestamp in epoch milliseconds, for logs and call records.
    ///
    /// Monotonic-derived: `anchor + elapsed_since_anchor`. Never decreasing.
    /// Not for behavioural decisions — deadlines/timers use `tokio::time`.
    pub fn now_ms(&self) -> i64 {
        self.anchor_wall_ms + self.anchor_instant.elapsed().as_millis() as i64
    }
}

/// Test helpers mirroring the source's virtual-time advance loop
/// (`tests/harness/runner.ts`). Behind the `testkit` feature so prod builds
/// never pull tokio's `test-util`.
#[cfg(feature = "testkit")]
pub mod testkit {
    use std::time::Duration;

    /// Advance paused tokio time in fixed `chunk`s up to `total`, awaiting
    /// between chunks so in-flight timer fibers observe intermediate values —
    /// the behaviour the source relied on by adjusting `TestClock` in 100 ms
    /// steps rather than one big jump (tokio's auto-advance otherwise leaps
    /// straight to the next deadline).
    pub async fn advance_in_chunks(total: Duration, chunk: Duration) {
        assert!(!chunk.is_zero(), "chunk must be non-zero");
        let mut remaining = total;
        while !remaining.is_zero() {
            let step = remaining.min(chunk);
            tokio::time::advance(step).await;
            remaining -= step;
        }
    }

    /// [`advance_in_chunks`] with the source's canonical 100 ms step.
    pub async fn advance_in_100ms_chunks(total: Duration) {
        advance_in_chunks(total, Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn now_ms_advances_in_lockstep_with_tokio_time() {
        let clock = Clock::test_at(1_000_000);
        assert_eq!(clock.now_ms(), 1_000_000);

        tokio::time::advance(Duration::from_millis(250)).await;
        assert_eq!(clock.now_ms(), 1_000_250);

        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(clock.now_ms(), 1_030_250);
    }

    #[tokio::test(start_paused = true)]
    async fn now_ms_is_monotonic_non_decreasing() {
        let clock = Clock::test_at(0);
        let mut last = clock.now_ms();
        for _ in 0..5 {
            tokio::time::advance(Duration::from_millis(100)).await;
            let now = clock.now_ms();
            assert!(now >= last, "{now} < {last}");
            last = now;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn clones_share_the_same_timeline() {
        let a = Clock::test_at(500);
        let b = a.clone();
        tokio::time::advance(Duration::from_millis(750)).await;
        assert_eq!(a.now_ms(), b.now_ms());
        assert_eq!(a.now_ms(), 1_250);
    }

    #[cfg(feature = "testkit")]
    #[tokio::test(start_paused = true)]
    async fn chunked_advance_lands_on_total_and_steps_through() {
        let clock = Clock::test_at(0);
        // 250 ms in 100 ms chunks → observable steps at 100, 200, 250.
        crate::testkit::advance_in_chunks(
            Duration::from_millis(250),
            Duration::from_millis(100),
        )
        .await;
        assert_eq!(clock.now_ms(), 250);
    }

    // The linear law that replaces the old draft's "deadline = f(injected now,
    // timeout)" property: deadlines are now monotonic, so the only thing to pin
    // about `now_ms` is that it is exactly `anchor + advanced`.
    proptest! {
        #[test]
        fn now_ms_equals_anchor_plus_advance(
            anchor in -1_000_000_000i64..1_000_000_000i64,
            advance_ms in 0u64..10_000_000u64,
        ) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .start_paused(true)
                .build()
                .unwrap();
            rt.block_on(async {
                let clock = Clock::test_at(anchor);
                tokio::time::advance(Duration::from_millis(advance_ms)).await;
                prop_assert_eq!(clock.now_ms(), anchor + advance_ms as i64);
                Ok(())
            })?;
        }
    }
}
