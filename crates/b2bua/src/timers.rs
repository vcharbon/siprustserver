//! `TimerService` — one `tokio_util::time::DelayQueue` driver holding every
//! pending B2BUA timer (keepalive, no-answer, max-duration, terminating-safety,
//! …). Port of `TimerService.ts`, reusing the single-`DelayQueue` shape from
//! sip-txn (ADR-0007) but with a B2BUA-local driver (sip-txn's queue is private
//! to its actor). Rides `tokio::time`, so `pause`/`advance` drives it in tests.
//!
//! On expiry the driver emits a [`CallEvent::Timer`] on its fire channel; the
//! router selects on that channel and routes it through the per-call dispatcher
//! exactly like an inbound message — so timer handling shares the per-call FIFO.
//!
//! ## Cancellation: epoch correctness **+** physical `Key` removal
//!
//! A `DelayQueue` `Key` is a bare slab index with no generation stamp: when an
//! entry expires or is removed, its slot is freed and the **next insert reuses
//! it, yielding the same `Key` value**. A *stale* `id → Key` map therefore
//! aliases, and `try_remove(stale_key)` then evicts whatever *live* timer now
//! occupies that slot — a silent, catastrophic wrong-timer cancel (the bug where
//! a keepalive-timeout cancel killed the rescheduled keepalive). See
//! `[[test-time clock & timers]]` in CLAUDE.md.
//!
//! The aliasing hazard needs a **stale** key — one held past the moment its
//! entry left the queue. We never hold one, so we *can* safely keep `Key`s and
//! physically remove. The driver is a single task (Schedule / Cancel / expiry
//! never interleave), and `active` is the authoritative record of queue
//! membership: every entry that expires is removed from `active` in the same
//! turn it fires, and every Cancel/CancelAll/reschedule removes its entry from
//! both `active` *and* the queue together. So a `Key` stored in `active` always
//! points at a still-queued entry — there is no stale-key window for
//! `try_remove` to alias into.
//!
//! Each entry therefore carries both a monotonic `epoch` and its `Key`:
//!
//! - **Physical removal (the why):** Cancel/CancelAll/reschedule call
//!   `try_remove(&key)` so a cancelled timer's slot is reclaimed *immediately*,
//!   not at its original deadline. Without this, a per-call `GlobalDuration`
//!   timer (default 1 h) cancelled by a 30 s call's BYE lingered ~3570 s as a
//!   tombstone; under steady load the queue grew to ≈ `arrival_rate × 3600`
//!   (~850k entries observed at ~100 cps) and the oversized timing wheel drove a
//!   monotonic CPU climb that looked like a leak but wasn't. Physical removal
//!   keeps `queue.len()` ≈ the live timer count. This is the concrete instance
//!   of the CLAUDE.md rule "all per-call state MUST be released at call end."
//! - **Epoch (the backstop):** a `Fired` is delivered only if its epoch still
//!   matches `active`. So even if a removal were ever missed, a superseded or
//!   cancelled entry that slipped through still drops as a tombstone instead of
//!   mis-firing. Correctness never depends on the removal having happened —
//!   `try_remove` only bounds the queue size.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use sip_clock::Clock;
use tokio::sync::mpsc;
use tokio_util::time::{delay_queue::Key, DelayQueue};

use call::{TimerEntry, TimerType};

use crate::event::CallEvent;
use crate::metrics::B2buaMetrics;

struct Fired {
    id: String,
    epoch: u64,
    timer_type: TimerType,
    call_ref: String,
    leg_id: Option<String>,
}

enum TimerCmd {
    Schedule { entry: TimerEntry, call_ref: String },
    Cancel { call_ref: String, id: String },
    CancelAll { call_ref: String },
}

/// Clone-cheap timer scheduling handle.
#[derive(Clone)]
pub struct TimerService {
    cmd_tx: mpsc::Sender<TimerCmd>,
}

impl TimerService {
    /// Spawn the driver with no metrics (the gauges are discarded). Tests use
    /// this; the production worker uses [`spawn_with_metrics`](Self::spawn_with_metrics).
    pub fn spawn(clock: Clock) -> (Self, mpsc::UnboundedReceiver<CallEvent>) {
        Self::spawn_with_metrics(clock, B2buaMetrics::default())
    }

    /// Spawn the driver, reporting the live timer-queue gauges
    /// (`b2bua_timer_queue_len` / `b2bua_timer_live`) into `metrics` on every
    /// state change. Returns the handle + the channel of fired timer events the
    /// router consumes.
    pub fn spawn_with_metrics(
        clock: Clock,
        metrics: B2buaMetrics,
    ) -> (Self, mpsc::UnboundedReceiver<CallEvent>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(1024);
        // The fire channel is UNBOUNDED on purpose: timer fires are already bounded
        // by real time and by the live `DelayQueue` entries (one per scheduled
        // timer). A bounded buffer smaller than the queue could silently drop a
        // keepalive/no-answer/max-duration fire on overflow — and the router's
        // single consumer awaits real I/O between drains, so a paused-clock advance
        // crossing many deadlines, or a scale burst, could overflow it. Load-
        // shedding still happens downstream at the bounded, *counted* per-call
        // dispatcher. Do NOT use a blocking `send().await` here: the driver shares
        // its task with the cmd channel, so blocking on a full fire channel would
        // deadlock against the router draining via cmd.
        let (fire_tx, fire_rx) = mpsc::unbounded_channel();
        tokio::spawn(driver(clock, cmd_rx, fire_tx, metrics));
        (Self { cmd_tx }, fire_rx)
    }

    pub async fn schedule(&self, entry: TimerEntry, call_ref: String) {
        let _ = self.cmd_tx.send(TimerCmd::Schedule { entry, call_ref }).await;
    }

    pub async fn cancel(&self, call_ref: String, id: String) {
        let _ = self.cmd_tx.send(TimerCmd::Cancel { call_ref, id }).await;
    }

    pub async fn cancel_all(&self, call_ref: String) {
        let _ = self.cmd_tx.send(TimerCmd::CancelAll { call_ref }).await;
    }

    /// Re-arm a hydrated call's persisted timer intents into this node's driver.
    ///
    /// Called once, on the fresh failover hydration of a call from the replica
    /// (`router::run` → `CallState::hydrate_from_replica` with `fresh == true`).
    /// The live `DelayQueue` is per-node and is NOT replicated, so a call
    /// materialized from a backup partition arrives with no live timers here;
    /// without this re-arm its keepalive never probes the peer and its
    /// duration/no-answer caps never reap it, so `active_calls` leaks on the
    /// takeover node (the failover analogue of the no-BYE leak).
    ///
    /// Each `TimerEntry.fire_at` is an **absolute wall deadline** (epoch ms),
    /// minted from `now_ms()` on whichever node first scheduled it (and replicated
    /// as part of the `Call`). The driver rebuilds a local monotonic timer as
    /// `(fire_at - now_ms()).max(0)`:
    ///
    /// - **Past-due** (`fire_at <= now_ms()`) → zero delay → fires on the next
    ///   driver tick, then routes through the rules exactly like any timer fire
    ///   (keepalive re-arms its next interval; a duration/no-answer cap reaps the
    ///   call). Nothing is dropped or recomputed.
    /// - Re-arm is idempotent: any later rule-emitted `ScheduleTimer` for the same
    ///   `(call_ref, id)` supersedes the restored entry via the driver's epoch bump.
    ///
    /// **Wall-time reliance (now bounded at the replication boundary).** `fire_at`
    /// came from the *dead* node's clock and is compared against *this* node's
    /// `now_ms()`, so the reconstructed deadline is only as accurate as the two
    /// nodes' wall clocks agree. That residual is now **corrected before restore**:
    /// the router's `sanitize_restored_timers` seam re-anchors every rehydrated
    /// `fire_at` by the `skew_offset_ms` the replication boundary persisted
    /// (`receiver_now − origin_now`), so this driver receives deadlines already in
    /// its own clock frame and its `fire_at − now_ms` math bounds skew to
    /// ~replication latency instead of trusting the dead node's clock unboundedly.
    /// The driver itself stays clock-skew-agnostic — NO re-anchor/smoothing logic
    /// lives here (ADR-0014); all of it is in the router hydration seam. This is the
    /// one place `now_ms()` is a behavioural, cross-node input — see the HA note in
    /// `sip-clock`'s crate docs. Under the simulated clock there is a single process
    /// and one paused `tokio::time` timeline, so `fire_at` and `now_ms()` ride the
    /// *same* `Instant` with zero real skew (the harness exercises the past-due path
    /// deterministically; the failover harness's per-node clock-anchor-offset knob
    /// now injects deterministic inter-node skew to exercise the re-anchor seam).
    pub async fn restore(&self, entries: Vec<TimerEntry>, call_ref: String) {
        for entry in entries {
            self.schedule(entry, call_ref.clone()).await;
        }
    }
}

async fn driver(
    clock: Clock,
    mut cmd_rx: mpsc::Receiver<TimerCmd>,
    fire_tx: mpsc::UnboundedSender<CallEvent>,
    metrics: B2buaMetrics,
) {
    let mut queue: DelayQueue<Fired> = DelayQueue::new();
    // The live `(epoch, Key)` per timer, keyed by `(call_ref, id)`. The timer
    // service is a SINGLE shared driver across every call, but timer ids are
    // per-call and collide across calls (every call's keepalive is `"Keepalive"`,
    // every keepalive-timeout is `"KeepaliveTimeout:a"`/`":b"`, etc.). Keying
    // `active` by id alone aliased calls: scheduling call N+1's `"Keepalive"`
    // overwrote the entry for call N's, so call N's queued entry was orphaned and
    // silently never fired — only the most-recently-scheduled call per id kept
    // its timers, so at scale keepalives stopped firing and dead-peer calls were
    // never reaped (active_calls grew unbounded). The `(call_ref, id)` key
    // isolates each call.
    //
    // The stored `Key` lets Cancel/CancelAll/reschedule physically `try_remove`
    // the queue entry instead of leaving it to expire as a tombstone — see the
    // module docs. The invariant that makes that safe: `active` always mirrors
    // queue membership (an entry is removed from `active` in the same turn it
    // fires), so a stored `Key` never points at a reused slot. The `epoch` stays
    // as a backstop: a `Fired` whose epoch no longer matches `active` is dropped,
    // so even a missed removal can never mis-fire. `by_call` indexes ids for
    // `CancelAll`.
    let mut active: HashMap<(String, String), (u64, Key)> = HashMap::new();
    let mut by_call: HashMap<String, HashSet<String>> = HashMap::new();
    let mut next_epoch: u64 = 0;

    // The gauges are written at the end of every loop iteration, but the driver
    // only iterates on a command or an expiry — so under low load with long-lived
    // timers a slowly-climbing tombstone backlog would be invisible to a scrape
    // until the next unrelated event. This tick re-publishes them on a fixed
    // cadence so `queue_len − live` stays a trustworthy regression alarm even at
    // idle. `Skip` collapses a backlog of missed ticks (e.g. after a long
    // paused-clock advance) into one, rather than firing a storm.
    let mut gauge_tick = tokio::time::interval(Duration::from_secs(5));
    gauge_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => match cmd {
                None => break, // all handles dropped
                Some(TimerCmd::Schedule { entry, call_ref }) => {
                    next_epoch += 1;
                    let epoch = next_epoch;
                    let akey = (call_ref.clone(), entry.id.clone());
                    // Re-arm: physically drop the previous queue entry for this
                    // (call_ref, id) so it can't linger. `try_remove` is safe —
                    // the stored Key is for a still-queued entry (active mirrors
                    // the queue), so it cannot alias a reused slot. The epoch bump
                    // still supersedes it logically as a backstop.
                    if let Some(&(_, old_key)) = active.get(&akey) {
                        queue.try_remove(&old_key);
                    }
                    by_call.entry(call_ref.clone()).or_default().insert(entry.id.clone());
                    // Rebuild the monotonic delay from the absolute wall deadline.
                    // Within a process this cancels — the rule minted `fire_at`
                    // from this same `now_ms()`, so delay == the original delay_ms
                    // regardless of the clock anchor. Across a failover `restore`,
                    // `fire_at` came from the *dead* node's clock, so the result
                    // depends on cross-node wall-clock agreement (see `restore`).
                    // Past-due (`fire_at <= now`) clamps to 0 → fires next tick.
                    let delay = (entry.fire_at - clock.now_ms()).max(0) as u64;
                    let key = queue.insert(
                        Fired {
                            id: entry.id,
                            epoch,
                            timer_type: entry.timer_type,
                            call_ref,
                            leg_id: entry.leg_id,
                        },
                        Duration::from_millis(delay),
                    );
                    active.insert(akey, (epoch, key));
                }
                Some(TimerCmd::Cancel { call_ref, id }) => {
                    // Physical cancel: forget this call's timer AND reclaim its
                    // queue slot now, so it never lingers as a tombstone.
                    if let Some((_, key)) = active.remove(&(call_ref.clone(), id.clone())) {
                        queue.try_remove(&key);
                    }
                    if let Some(ids) = by_call.get_mut(&call_ref) {
                        ids.remove(&id);
                        // Don't leave an empty set keyed by a now-timerless call:
                        // only CancelAll reaps `by_call`, so a call that only ever
                        // individual-Cancels would strand an empty entry forever.
                        if ids.is_empty() {
                            by_call.remove(&call_ref);
                        }
                    }
                }
                Some(TimerCmd::CancelAll { call_ref }) => {
                    // Call teardown: every per-call timer must leave both `active`
                    // AND the queue (the "all per-call state released at call end"
                    // guarantee). This is the cancel that frees the long-lived
                    // GlobalDuration slot a clean BYE would otherwise strand.
                    if let Some(ids) = by_call.remove(&call_ref) {
                        for id in ids {
                            if let Some((_, key)) = active.remove(&(call_ref.clone(), id)) {
                                queue.try_remove(&key);
                            }
                        }
                    }
                }
            },
            expired = next_expired(&mut queue), if !queue.is_empty() => {
                // Deliver only the live generation. `poll_expired` already removed
                // this entry from the queue (its `Key`/slot is now freed), so we
                // only clear `active` — never `try_remove` it. With physical
                // cancellation a surviving tombstone is rare (a removal missed),
                // but the epoch backstop still drops it. (Not a `continue`: the
                // gauge update below must run on every iteration.)
                let akey = (expired.call_ref.clone(), expired.id.clone());
                if active.get(&akey).map(|&(e, _)| e) == Some(expired.epoch) {
                    active.remove(&akey);
                    if let Some(ids) = by_call.get_mut(&expired.call_ref) {
                        ids.remove(&expired.id);
                        if ids.is_empty() {
                            by_call.remove(&expired.call_ref);
                        }
                    }
                    let event = CallEvent::Timer {
                        timer_type: expired.timer_type,
                        call_ref: expired.call_ref,
                        leg_id: expired.leg_id,
                    };
                    // Unbounded: only fails if the router (receiver) is gone — i.e.
                    // the worker is shutting down — and dropping the fire is fine.
                    let _ = fire_tx.send(event);
                }
            }
            // Idle heartbeat: wake periodically purely to re-publish the gauges
            // below (no state change of its own).
            _ = gauge_tick.tick() => {}
        }
        // Live timer-queue gauges. `queue.len()` is the physical entry count;
        // `active.len()` is the schedulable timer count. With physical
        // cancellation the two now track each other — their gap is the
        // tombstone backlog, which should stay ≈ 0. A gap that *climbs* means a
        // removal is being missed and entries are lingering again (the old 1 h
        // GlobalDuration leak); it is the regression alarm for this fix. Updated
        // every iteration (cheap Relaxed stores); the driver only iterates on a
        // state change.
        metrics.set_timer_gauges(queue.len() as u64, active.len() as u64);
    }
}

/// Await the next expired timer. Only polled while the queue is non-empty (an
/// empty `DelayQueue` resolves to `Ready(None)` and would busy-spin `select!`).
async fn next_expired(q: &mut DelayQueue<Fired>) -> Fired {
    std::future::poll_fn(|cx| q.poll_expired(cx))
        .await
        .expect("guarded by !is_empty()")
        .into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn fires_after_delay_under_paused_clock() {
        let clock = Clock::test_at(0);
        let (timers, mut fire_rx) = TimerService::spawn(clock);
        timers
            .schedule(
                TimerEntry {
                    id: "t1".into(),
                    timer_type: TimerType::NoAnswer,
                    fire_at: 5_000,
                    leg_id: Some("b-1".into()),
                },
                "w0|cid|tag".into(),
            )
            .await;
        // Nothing yet.
        tokio::time::advance(Duration::from_millis(4_000)).await;
        assert!(fire_rx.try_recv().is_err());
        // Cross the deadline.
        tokio::time::advance(Duration::from_millis(1_500)).await;
        let ev = fire_rx.recv().await.unwrap();
        match ev {
            CallEvent::Timer { timer_type, call_ref, leg_id } => {
                assert_eq!(timer_type, TimerType::NoAnswer);
                assert_eq!(call_ref, "w0|cid|tag");
                assert_eq!(leg_id.as_deref(), Some("b-1"));
            }
            _ => panic!("expected timer event"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_prevents_fire() {
        let clock = Clock::test_at(0);
        let (timers, mut fire_rx) = TimerService::spawn(clock);
        timers
            .schedule(
                TimerEntry { id: "t1".into(), timer_type: TimerType::Keepalive, fire_at: 1_000, leg_id: None },
                "c".into(),
            )
            .await;
        timers.cancel("c".into(), "t1".into()).await;
        tokio::time::advance(Duration::from_millis(2_000)).await;
        // Give the driver a tick to process.
        tokio::task::yield_now().await;
        assert!(fire_rx.try_recv().is_err());
    }

    /// Regression for the `DelayQueue` `Key`-aliasing bug (the keepalive cycle-2
    /// hang). Replays the real sequence: a timer fires (freeing its slab slot),
    /// a second timer reuses that slot, the first is rescheduled, then the second
    /// is cancelled. Under the old `try_remove`-by-`Key` driver the cancel
    /// aliased the freed slot and evicted the *rescheduled* first timer, which
    /// then silently never fired. With logical (epoch) cancellation it must fire.
    #[tokio::test(start_paused = true)]
    async fn reschedule_survives_aliasing_cancel() {
        let clock = Clock::test_at(0);
        let (timers, mut fire_rx) = TimerService::spawn(clock);
        let cref = "w0|cid|tag".to_string();

        // 1. Arm "keepalive" for t=30s and let it fire (slot freed).
        timers
            .schedule(
                TimerEntry { id: "keepalive".into(), timer_type: TimerType::Keepalive, fire_at: 30_000, leg_id: None },
                cref.clone(),
            )
            .await;
        tokio::time::advance(Duration::from_millis(30_000)).await;
        assert!(
            matches!(fire_rx.recv().await, Some(CallEvent::Timer { timer_type: TimerType::Keepalive, .. })),
            "first keepalive fires at 30s",
        );

        // 2. Arm a per-leg timeout (reuses the freed slot) ...
        timers
            .schedule(
                TimerEntry { id: "KeepaliveTimeout:a".into(), timer_type: TimerType::KeepaliveTimeout, fire_at: 35_000, leg_id: Some("a".into()) },
                cref.clone(),
            )
            .await;
        // 3. ... reschedule keepalive for t=60s ...
        timers
            .schedule(
                TimerEntry { id: "keepalive".into(), timer_type: TimerType::Keepalive, fire_at: 60_000, leg_id: None },
                cref.clone(),
            )
            .await;
        // 4. ... and cancel the timeout (the aliasing trigger in the old driver).
        timers.cancel(cref.clone(), "KeepaliveTimeout:a".into()).await;

        // The cancelled timeout must NOT fire; the rescheduled keepalive MUST.
        tokio::time::advance(Duration::from_millis(30_000)).await; // → t=60s
        let ev = fire_rx.recv().await;
        assert!(
            matches!(ev, Some(CallEvent::Timer { timer_type: TimerType::Keepalive, .. })),
            "rescheduled keepalive survives the aliasing cancel and fires at 60s, got {ev:?}",
        );
        // Nothing else pending (the timeout was a dropped tombstone).
        assert!(fire_rx.try_recv().is_err(), "cancelled timeout must not fire");
    }

    /// Let the single-task driver drain pending commands and run its end-of-loop
    /// gauge update. `schedule`/`cancel` only enqueue; yielding hands the runtime
    /// to the driver task (current-thread under `start_paused`).
    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    /// The fix for the timer-queue tombstone CPU drift: cancelling a timer must
    /// physically reclaim its `DelayQueue` slot *now*, not leave it to expire at
    /// its original deadline. Models the real leak — a long `GlobalDuration`
    /// timer (1 h) armed on a call that tears down seconds later: under the old
    /// logical-only cancel the slot lingered ~1 h, so at load the queue grew to
    /// hundreds of thousands of dead entries. `queue_len` must drop to 0 on the
    /// teardown cancel, well before the 1 h deadline.
    #[tokio::test(start_paused = true)]
    async fn cancel_physically_reclaims_the_queue_slot() {
        let clock = Clock::test_at(0);
        let metrics = B2buaMetrics::new();
        let (timers, _fire_rx) = TimerService::spawn_with_metrics(clock, metrics.clone());

        timers
            .schedule(
                TimerEntry { id: "GlobalDuration".into(), timer_type: TimerType::GlobalDuration, fire_at: 3_600_000, leg_id: None },
                "c".into(),
            )
            .await;
        settle().await;
        assert_eq!(metrics.timer_queue_len(), 1, "armed timer occupies a queue slot");
        assert_eq!(metrics.timer_live(), 1);

        // Call teardown — the slot must be freed immediately, not at +1 h.
        timers.cancel_all("c".into()).await;
        settle().await;
        assert_eq!(metrics.timer_queue_len(), 0, "CancelAll reclaims the slot — no lingering tombstone");
        assert_eq!(metrics.timer_live(), 0, "no schedulable timers remain");
    }

    /// A reschedule must also reclaim the superseded slot, so a periodically
    /// re-armed timer (the keepalive) keeps `queue_len` flat instead of leaking
    /// one tombstone per re-arm.
    #[tokio::test(start_paused = true)]
    async fn reschedule_does_not_accumulate_tombstones() {
        let clock = Clock::test_at(0);
        let metrics = B2buaMetrics::new();
        let (timers, _fire_rx) = TimerService::spawn_with_metrics(clock, metrics.clone());

        for round in 0..50 {
            timers
                .schedule(
                    TimerEntry { id: "keepalive".into(), timer_type: TimerType::Keepalive, fire_at: 300_000 + round, leg_id: None },
                    "c".into(),
                )
                .await;
        }
        settle().await;
        assert_eq!(metrics.timer_queue_len(), 1, "50 re-arms collapse to one live entry, not 50 tombstones");
        assert_eq!(metrics.timer_live(), 1);
    }

    /// Review regression (#8): a burst of more timers than the old bounded fire
    /// channel held (1024) must deliver EVERY fire — the `DelayQueue` is the only
    /// bound, not an in-front buffer. Under the old `try_send` into `channel(1024)`
    /// the overflow was silently dropped (a lost keepalive/no-answer/max-duration).
    #[tokio::test(start_paused = true)]
    async fn timer_flood_past_old_channel_cap_delivers_every_fire() {
        let clock = Clock::test_at(0);
        let (timers, mut fire_rx) = TimerService::spawn(clock);

        const N: usize = 3_000; // comfortably past the old 1024 cap
        for i in 0..N {
            timers
                .schedule(
                    TimerEntry {
                        id: format!("t{i}"),
                        timer_type: TimerType::Keepalive,
                        fire_at: 1_000,
                        leg_id: None,
                    },
                    format!("call-{i}"),
                )
                .await;
        }

        // Cross the single deadline so all N fire in one go, with NO interleaved
        // draining (the worst case the bounded channel dropped on).
        tokio::time::advance(Duration::from_millis(1_500)).await;

        // Drain, letting the driver flush the DelayQueue between empties. Stop
        // when all N are in or the channel stays empty across many settle passes.
        let mut got = 0;
        let mut idle = 0;
        while got < N && idle < 64 {
            match fire_rx.try_recv() {
                Ok(_) => {
                    got += 1;
                    idle = 0;
                }
                Err(_) => {
                    idle += 1;
                    tokio::task::yield_now().await;
                }
            }
        }
        assert_eq!(got, N, "every fire delivered — no silent overflow drop");
    }

    /// Regression for the cross-call timer-id aliasing reap bug. The timer
    /// service is a single shared driver, but timer ids are per-call and repeat
    /// across calls (every established call arms a `"Keepalive"` timer). With the
    /// old id-only `active` map, scheduling a second call's `"Keepalive"` (same
    /// id, different call_ref) overwrote the first's live epoch, so the first
    /// call's queued keepalive became a stale tombstone and silently never
    /// fired — at scale keepalives stopped, dead peers were never probed, and
    /// `active_calls` grew without bound. Both calls' identically-named timers
    /// must now fire independently.
    #[tokio::test(start_paused = true)]
    async fn colliding_timer_ids_across_calls_both_fire() {
        let clock = Clock::test_at(0);
        let (timers, mut fire_rx) = TimerService::spawn(clock);

        // Two distinct calls, IDENTICAL timer id — the production shape.
        let call_a = "w0|call-a|tag-a".to_string();
        let call_b = "w0|call-b|tag-b".to_string();
        for cref in [&call_a, &call_b] {
            timers
                .schedule(
                    TimerEntry {
                        id: "Keepalive".into(),
                        timer_type: TimerType::Keepalive,
                        fire_at: 30_000,
                        leg_id: None,
                    },
                    cref.clone(),
                )
                .await;
        }

        tokio::time::advance(Duration::from_millis(30_000)).await;

        let mut fired: Vec<String> = Vec::new();
        for _ in 0..2 {
            match fire_rx.recv().await {
                Some(CallEvent::Timer { timer_type: TimerType::Keepalive, call_ref, .. }) => {
                    fired.push(call_ref)
                }
                other => panic!("expected a keepalive fire, got {other:?}"),
            }
        }
        fired.sort();
        assert_eq!(
            fired,
            vec![call_a, call_b],
            "both calls' keepalives fire — no cross-call aliasing tombstone",
        );
    }

    /// Cancelling one call's timer must not cancel another call's identically
    /// named timer. With the old id-only cancel, `cancel("Keepalive")` for call A
    /// wiped call B's `"Keepalive"` epoch too.
    #[tokio::test(start_paused = true)]
    async fn cancel_is_scoped_to_its_call() {
        let clock = Clock::test_at(0);
        let (timers, mut fire_rx) = TimerService::spawn(clock);
        let call_a = "w0|call-a|tag-a".to_string();
        let call_b = "w0|call-b|tag-b".to_string();
        for cref in [&call_a, &call_b] {
            timers
                .schedule(
                    TimerEntry {
                        id: "Keepalive".into(),
                        timer_type: TimerType::Keepalive,
                        fire_at: 30_000,
                        leg_id: None,
                    },
                    cref.clone(),
                )
                .await;
        }
        // Cancel only call A's keepalive.
        timers.cancel(call_a.clone(), "Keepalive".into()).await;

        tokio::time::advance(Duration::from_millis(30_000)).await;

        // Exactly call B's keepalive must fire.
        match fire_rx.recv().await {
            Some(CallEvent::Timer { timer_type: TimerType::Keepalive, call_ref, .. }) => {
                assert_eq!(call_ref, call_b, "call B's keepalive survives call A's cancel");
            }
            other => panic!("expected call B's keepalive, got {other:?}"),
        }
        tokio::task::yield_now().await;
        assert!(fire_rx.try_recv().is_err(), "call A's keepalive stays cancelled");
    }
}
