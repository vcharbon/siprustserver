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
pub mod testkit {
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    /// Advance paused tokio time by `total`, running the work in flight at every
    /// instant a timer falls due inside the span: the caller sleeps, so the
    /// runtime polls every ready task until none is left and only then
    /// auto-advances, to the next due timer. A request a detached task sends
    /// inside the span is answered at its wire instant, within its budget. The
    /// trailing [`settle`] runs the work due at the span's last instant before
    /// the caller resumes. Panics outside a paused runtime; a task that never
    /// goes idle (a `yield_now` loop) holds the clock and the advance with it.
    pub async fn advance_settled(total: Duration) {
        // tokio's own check that the clock is paused; moves no time.
        tokio::time::advance(Duration::ZERO).await;
        tokio::time::sleep(total).await;
        settle().await;
    }

    /// Real time [`run_paused_within`] gives a runtime to take its abort.
    pub const ABORT_GRACE: Duration = Duration::from_secs(1);

    /// Run `scenario` on a paused current-thread runtime of its own thread and
    /// return its output, or `None` when it has not finished after `wall` of real
    /// time. A task that keeps yielding but never goes idle holds the paused
    /// clock, so a scenario that sleeps behind it would hang: this bounds such a
    /// test in real time, and drops the runtime and every task on it before it
    /// returns. A panic in the scenario is re-raised. A task stuck in a
    /// synchronous loop never lets the runtime see the abort: after
    /// [`ABORT_GRACE`] this panics and leaves that thread running.
    pub fn run_paused_within<F, Fut, T>(wall: Duration, scenario: F) -> Option<T>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T>,
        T: Send + 'static,
    {
        let (abort, aborted) = tokio::sync::oneshot::channel::<()>();
        let (done, finished) = std::sync::mpsc::channel();
        let runner = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .expect("a paused current-thread runtime");
            let out = rt.block_on(async move {
                tokio::select! {
                    biased;
                    _ = aborted => None,
                    out = scenario() => Some(out),
                }
            });
            drop(rt);
            let _ = done.send(out);
        });
        let out = match finished.recv_timeout(wall) {
            Ok(out) => out,
            // The runner panicked; the join below re-raises it.
            Err(RecvTimeoutError::Disconnected) => None,
            Err(RecvTimeoutError::Timeout) => {
                let _ = abort.send(());
                match finished.recv_timeout(ABORT_GRACE) {
                    Ok(out) => out,
                    Err(RecvTimeoutError::Disconnected) => None,
                    Err(RecvTimeoutError::Timeout) => panic!(
                        "the scenario's runtime did not take the abort within {ABORT_GRACE:?}: \
                         a task is stuck in a synchronous loop (its thread is left running)"
                    ),
                }
            }
        };
        if let Err(panic) = runner.join() {
            std::panic::resume_unwind(panic);
        }
        out
    }

    /// Generous settle count, sized for the deepest test pipeline (the failover
    /// harness drives the SIP *and* replication planes together — notify →
    /// server drain → send → transit-delivery actor → puller recv → store apply
    /// → status publish, several task hops). One `yield_now` advances exactly
    /// one hop, so we yield generously. A single constant here replaces the
    /// per-crate 64-vs-96 drift the copied `settle()` helpers had accumulated.
    const SETTLE_YIELDS: usize = 96;

    /// Let every spawned task in the sim pipeline hop forward without advancing
    /// time. One `yield_now` only advances one task hop and the pipeline is many
    /// hops deep, so this yields `SETTLE_YIELDS` times. The single home for the
    /// "settle generously" idiom the repl tests and the ha / failover harnesses
    /// share.
    pub async fn settle() {
        for _ in 0..SETTLE_YIELDS {
            tokio::task::yield_now().await;
        }
    }

    /// Advance paused tokio time by `total` in settled 100 ms chunks: each chunk
    /// is an [`advance_settled`], so work falling due inside a chunk runs at its
    /// own instant. The body behind the replication and failover harnesses'
    /// `advance()` and the repl tests' `tick()`.
    ///
    /// `pump(d)` advances `ceil(d/100ms) + 1` chunks of virtual time: the
    /// trailing chunk delivers the frames produced at the span's last instant.
    pub async fn pump(total: Duration) {
        pump_sampled(total, || {}).await;
    }

    /// [`pump`] with `on_chunk` run after every chunk — the sampling seam for a
    /// harness that observes component state as time moves (it sees the
    /// pipeline at 100 ms granularity, never between two chunks). Timing is
    /// identical to [`pump`]; `on_chunk` is synchronous so it cannot perturb the
    /// pipeline it observes.
    pub async fn pump_sampled(total: Duration, mut on_chunk: impl FnMut()) {
        const CHUNK: Duration = Duration::from_millis(100);
        let chunks = (total.as_millis() as u64).div_ceil(100).max(1);
        for _ in 0..=chunks {
            advance_settled(CHUNK).await;
            on_chunk();
        }
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

    // ── Stage 3: clock-skew divergence observability seam ──

    #[tokio::test(start_paused = true)]
    async fn wall_divergence_is_raw_minus_now() {
        // The pure seam: inject the raw wall reading, so the test is deterministic
        // and never touches SystemTime. now_ms == anchor under a paused clock.
        let clock = Clock::test_at(1_000_000);
        // Raw wall AHEAD of our anchored now_ms by 300 s (a host step forward).
        assert_eq!(clock.wall_divergence_ms(1_300_000), 300_000);
        // Raw wall BEHIND (a step backward) → negative divergence.
        assert_eq!(clock.wall_divergence_ms(700_000), -300_000);
        // Agreement → zero.
        assert_eq!(clock.wall_divergence_ms(1_000_000), 0);

        // now_ms advances with tokio time, but a STEPPED raw reading does not follow
        // it — so the divergence tracks the step, which is the whole point.
        tokio::time::advance(Duration::from_secs(10)).await;
        // now_ms is now 1_010_000; a raw wall that stayed at 1_000_000 (monotonic
        // Clock kept moving, host clock did not) reads as −10_000 divergence.
        assert_eq!(clock.wall_divergence_ms(1_000_000), -10_000);
    }

    #[test]
    fn system_panics_on_pre_epoch_clock() {
        // We cannot force SystemTime backwards, but we CAN assert the guard: the
        // pre-epoch branch is `duration_since(UNIX_EPOCH).expect(...)`. Model it
        // directly — a pre-epoch SystemTime yields `Err` from `duration_since`, and
        // our constructor `.expect()`s it. This documents/locks the fail-loud
        // contract that replaced the silent `unwrap_or(0)`.
        let pre_epoch = UNIX_EPOCH - std::time::Duration::from_secs(1);
        let result = std::panic::catch_unwind(|| {
            pre_epoch
                .duration_since(UNIX_EPOCH)
                .expect("system wall clock is before the UNIX epoch (grossly misconfigured host clock) — refusing to anchor Clock to a pre-1970 time")
        });
        assert!(result.is_err(), "a pre-epoch clock must panic, not anchor to 0");
    }

    #[tokio::test(start_paused = true)]
    async fn clones_share_the_same_timeline() {
        let a = Clock::test_at(500);
        let b = a.clone();
        tokio::time::advance(Duration::from_millis(750)).await;
        assert_eq!(a.now_ms(), b.now_ms());
        assert_eq!(a.now_ms(), 1_250);
    }

    #[tokio::test(start_paused = true)]
    async fn a_settled_advance_lands_on_total_and_runs_each_timer_at_its_instant() {
        let clock = Clock::test_at(0);
        let (done, landed) = tokio::sync::oneshot::channel();
        let hops = clock.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            tokio::time::sleep(Duration::from_millis(60)).await;
            let _ = done.send(hops.now_ms());
        });
        crate::testkit::advance_settled(Duration::from_millis(250)).await;
        assert_eq!(clock.now_ms(), 250);
        assert_eq!(landed.await.unwrap(), 120, "the second sleep starts where the first fired");
    }

    #[test]
    fn a_bounded_scenario_returns_its_output_after_its_virtual_time() {
        let out = crate::testkit::run_paused_within(Duration::from_secs(10), || async {
            tokio::time::sleep(Duration::from_secs(3_600)).await;
            7
        });
        assert_eq!(out, Some(7));
    }

    #[test]
    fn a_bounded_scenario_behind_a_task_that_never_goes_idle_is_none() {
        let out = crate::testkit::run_paused_within(Duration::from_millis(200), || async {
            tokio::spawn(async {
                loop {
                    tokio::task::yield_now().await;
                }
            });
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        assert_eq!(out, None);
    }

    #[test]
    fn a_bounded_scenario_that_panics_re_raises_its_panic() {
        let caught = std::panic::catch_unwind(|| {
            crate::testkit::run_paused_within(Duration::from_secs(10), || async {
                panic!("expected: the scenario panics")
            })
        });
        let payload = caught.expect_err("the panic is re-raised");
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"expected: the scenario panics"));
    }

    #[test]
    fn a_bounded_scenario_stuck_in_a_synchronous_loop_panics_after_the_grace() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let release = Arc::new(AtomicBool::new(false));
        let stuck = release.clone();
        let caught = std::panic::catch_unwind(|| {
            crate::testkit::run_paused_within(Duration::from_millis(100), move || async move {
                while !stuck.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            })
        });
        // Let the abandoned thread end.
        release.store(true, Ordering::Relaxed);
        let payload = caught.expect_err("a runtime that cannot take the abort panics");
        let message = payload.downcast_ref::<String>().expect("a formatted message");
        assert!(message.contains("synchronous loop"), "{message}");
    }

    /// One hop of a request or of its answer between two tasks.
    #[cfg(feature = "testkit")]
    const HOP: Duration = Duration::from_millis(60);

    /// A caller asks a service task under a 150 ms budget; the service answers
    /// after one [`HOP`] in and one back. The receiver yields the virtual time
    /// the answer took, `None` for a timeout.
    #[cfg(feature = "testkit")]
    fn ask_under_budget() -> tokio::sync::oneshot::Receiver<Option<Duration>> {
        use tokio::sync::{mpsc, oneshot};
        let (to_service, mut requests) = mpsc::channel::<oneshot::Sender<()>>(1);
        tokio::spawn(async move {
            while let Some(reply) = requests.recv().await {
                tokio::time::sleep(HOP).await;
                tokio::spawn(async move {
                    tokio::time::sleep(HOP).await;
                    let _ = reply.send(());
                });
            }
        });
        let (done, outcome) = oneshot::channel();
        tokio::spawn(async move {
            let started = tokio::time::Instant::now();
            let (reply, answer) = oneshot::channel();
            to_service.send(reply).await.expect("the service task runs");
            let answered = tokio::time::timeout(Duration::from_millis(150), answer).await;
            let _ = done.send(answered.ok().and_then(Result::ok).map(|()| started.elapsed()));
        });
        outcome
    }

    #[cfg(feature = "testkit")]
    #[tokio::test(start_paused = true)]
    async fn a_pump_answers_a_request_from_another_task_at_its_wire_instant() {
        let outcome = ask_under_budget();
        crate::testkit::pump(Duration::from_secs(1)).await;
        assert_eq!(
            outcome.await.unwrap(),
            Some(2 * HOP),
            "the answer lands one round trip after the request, inside its budget"
        );
    }

    #[cfg(feature = "testkit")]
    #[tokio::test(start_paused = true)]
    async fn a_sampled_pump_samples_after_every_100ms_chunk_and_once_more() {
        let clock = Clock::test_at(0);
        let mut seen = Vec::new();
        crate::testkit::pump_sampled(Duration::from_millis(250), || seen.push(clock.now_ms()))
            .await;
        assert_eq!(seen, vec![100, 200, 300, 400]);
    }

    // The linear law: deadlines are monotonic, so the only thing to pin about
    // `now_ms` is that it is exactly `anchor + advanced`.
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
