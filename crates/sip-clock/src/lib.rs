//! sip-clock — the clock seam (re-expression of the source's Effect `Clock` /
//! `TestClock`).
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
//! # HA note — failover timer reconstruction
//!
//! Monotonic `Instant`s are not portable across processes / restarts / replicas,
//! so a replicated timer can never ship a raw `Instant`. Replicated timers use
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
//!
//! ## Skew re-anchoring at the replication boundary (clock-skew hardening, landed)
//!
//! The raw cross-node trust above is now **bounded to ~replication latency**. The
//! replication `Data` frame carries an `origin_now_ms` stamp (the sender's
//! `now_ms()` at flush-send time); the receiving store persists
//! `skew_offset_ms = receiver_now_ms − origin_now_ms` alongside the body's
//! `expiry_at_ms` (the SAME re-anchor idiom already used for `body_ttl_ms`). On
//! failover/reclaim the B2BUA router's single restore-hygiene seam
//! (`router::sanitize_restored_timers`) adds that offset back to every rehydrated
//! `TimerEntry.fire_at` before re-arming it, so the deadline lands in the LIVE
//! node's clock frame regardless of a host NTP step that anchored the two pods on
//! opposite sides. A small deadband ignores sub-second offsets (dominated by
//! transit latency, not real skew). This is **accuracy only** — `(p,b)`-causal
//! reconciliation (ADR-0014) remains the sole correctness mechanism; the boundary
//! re-anchor introduces no wall-clock-dependent rule, settle window, or handback.
//! A periodic `clock_wall_divergence_ms` gauge makes a live host step observable
//! before it corrupts a restore. Keep the infra discipline too (slewing chrony,
//! host kept awake) — the SUT now bounds the residual, it does not license drift.

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
        // FAIL LOUDLY on a pre-epoch clock instead of silently anchoring to 1970
        // (`unwrap_or(0)`): a node that anchored at 0 would compute a
        // ~55-year skew offset against every healthy peer, so every replicated
        // timer it reclaimed would be reaped or deferred by decades — a silent,
        // catastrophic corruption. A `SystemTime` before 1970 in production is a
        // grossly-misconfigured host clock; crashing at boot is the correct,
        // visible failure (k8s restarts the pod; the operator sees CrashLoopBackOff
        // instead of a mysterious cluster-wide reclaim storm days later).
        let anchor_wall_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system wall clock is before the UNIX epoch (grossly misconfigured host clock) — refusing to anchor Clock to a pre-1970 time")
            .as_millis() as i64;
        Self { anchor_wall_ms, anchor_instant: Instant::now() }
    }

    /// Test constructor: pin the wall anchor to a fixed epoch-ms value.
    ///
    /// Under a paused runtime (`#[tokio::test(start_paused = true)]` or
    /// `tokio::time::pause()`), `now_ms()` then advances in lockstep with
    /// `tokio::time::advance`, giving fully deterministic timestamps.
    pub fn test_at(anchor_wall_ms: i64) -> Self {
        Self { anchor_wall_ms, anchor_instant: Instant::now() }
    }

    /// Wall-ish timestamp in epoch milliseconds, for logs and call records.
    ///
    /// Monotonic-derived: `anchor + elapsed_since_anchor`. Never decreasing.
    /// Not for behavioural decisions — deadlines/timers use `tokio::time`.
    pub fn now_ms(&self) -> i64 {
        self.anchor_wall_ms + self.anchor_instant.elapsed().as_millis() as i64
    }

    /// **Divergence** between a freshly-read raw wall clock and this monotonic-
    /// anchored `now_ms()`: `raw_wall_ms − now_ms()` (clock-skew hardening
    /// observability). This is the **pure, testable seam** behind the periodic
    /// `clock_wall_divergence_ms` gauge — inject the wall reading so the test does
    /// not depend on `SystemTime`.
    ///
    /// It measures how far this `Clock`'s anchored timeline has drifted from true
    /// wall time since it was anchored — either the anchor's own monotonic drift
    /// over long uptime, OR (the case that matters) a **host NTP step**: when the
    /// host clock jumps, `now_ms()` (monotonic-derived) does NOT follow, so a fresh
    /// `SystemTime::now()` reading and `now_ms()` diverge by the step size. A
    /// large, sudden magnitude names exactly the event that skews replicated timer
    /// deadlines across pods, turning a
    /// days-later failover mystery into a live signal. Do NOT re-anchor `Clock`
    /// from this — timestamps must stay monotonic; the behavioural correction is
    /// the replication-boundary re-anchor, not a clock rewrite.
    pub fn wall_divergence_ms(&self, raw_wall_ms: i64) -> i64 {
        raw_wall_ms - self.now_ms()
    }
}

/// Read the raw system wall clock in epoch ms (NOT monotonic-anchored) for the
/// `clock_wall_divergence_ms` sampler — the one place that deliberately reads
/// `SystemTime` to compare against a [`Clock`]. Returns `0` on a pre-epoch clock
/// (the sampler is observability-only; unlike [`Clock::system`] it must never
/// panic a running worker).
pub fn raw_system_wall_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Test helpers driving the paused tokio clock. Behind the `testkit` feature so
/// prod builds never pull tokio's `test-util`; this crate's own tests always
/// build them (its dev-dependencies carry `test-util`).
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

#[cfg(test)]
mod tests;
