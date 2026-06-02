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
//! ## Cancellation is logical (epoch/tombstone), never by `DelayQueue` `Key`
//!
//! A `DelayQueue` `Key` is a bare slab index with no generation stamp: when an
//! entry expires or is removed, its slot is freed and the **next insert reuses
//! it, yielding the same `Key` value**. A side `id → Key` map therefore aliases
//! the moment any cleanup is missed, and `try_remove(stale_key)` then evicts
//! whatever *live* timer now occupies that slot — a silent, catastrophic
//! wrong-timer cancel (this is the bug where a keepalive-timeout cancel killed
//! the rescheduled keepalive). See `[[test-time clock & timers]]` in CLAUDE.md.
//!
//! So we do **not** keep `Key`s and never call `try_remove`. Each scheduled
//! timer carries a monotonic `epoch`; the live epoch per id lives in `active`.
//! Cancel/CancelAll are pure map removals; a re-`Schedule` just bumps the epoch.
//! On expiry a `Fired` is delivered only if its epoch still matches `active`,
//! else it is a tombstone (superseded/cancelled) and dropped. Correctness rests
//! on a single invariant — `active[id] == fired.epoch` — with no `Key` to alias.
//! Cost: a cancelled/rescheduled entry lingers in the queue until its original
//! deadline, then drops harmlessly. Bounded and short-lived for SIP timers.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use sip_clock::Clock;
use tokio::sync::mpsc;
use tokio_util::time::DelayQueue;

use call::{TimerEntry, TimerType};

use crate::event::CallEvent;

struct Fired {
    id: String,
    epoch: u64,
    timer_type: TimerType,
    call_ref: String,
    leg_id: Option<String>,
}

enum TimerCmd {
    Schedule { entry: TimerEntry, call_ref: String },
    Cancel { id: String },
    CancelAll { call_ref: String },
}

/// Clone-cheap timer scheduling handle.
#[derive(Clone)]
pub struct TimerService {
    cmd_tx: mpsc::Sender<TimerCmd>,
}

impl TimerService {
    /// Spawn the driver. Returns the handle + the channel of fired timer events
    /// the router consumes.
    pub fn spawn(clock: Clock) -> (Self, mpsc::UnboundedReceiver<CallEvent>) {
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
        tokio::spawn(driver(clock, cmd_rx, fire_tx));
        (Self { cmd_tx }, fire_rx)
    }

    pub async fn schedule(&self, entry: TimerEntry, call_ref: String) {
        let _ = self.cmd_tx.send(TimerCmd::Schedule { entry, call_ref }).await;
    }

    pub async fn cancel(&self, id: String) {
        let _ = self.cmd_tx.send(TimerCmd::Cancel { id }).await;
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
    /// **Wall-time reliance.** Because `fire_at` came from the *dead* node's clock
    /// and is compared against *this* node's `now_ms()`, the reconstructed deadline
    /// is only as accurate as the two nodes' wall clocks agree (keep them NTP-
    /// disciplined). This is the one place `now_ms()` is a behavioural, cross-node
    /// input — see the HA note in `sip-clock`'s crate docs. Under the simulated
    /// clock there is a single process and one paused `tokio::time` timeline, so
    /// `fire_at` and `now_ms()` ride the *same* `Instant` with zero skew: the
    /// past-due path is exercised deterministically, but the cross-node skew
    /// dimension is NOT covered by the harness.
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
) {
    let mut queue: DelayQueue<Fired> = DelayQueue::new();
    // The live epoch per timer id. An id absent from `active` is cancelled; an
    // id whose epoch differs from a fired entry's was rescheduled (the old
    // entry is a tombstone). `by_call` indexes ids for `CancelAll`.
    let mut active: HashMap<String, u64> = HashMap::new();
    let mut by_call: HashMap<String, HashSet<String>> = HashMap::new();
    let mut next_epoch: u64 = 0;

    loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => match cmd {
                None => break, // all handles dropped
                Some(TimerCmd::Schedule { entry, call_ref }) => {
                    // Re-arm = bump the epoch; the previous queue entry (if any)
                    // becomes a tombstone and is filtered when it expires. No
                    // `try_remove`, so no `Key` aliasing is possible.
                    next_epoch += 1;
                    let epoch = next_epoch;
                    active.insert(entry.id.clone(), epoch);
                    by_call.entry(call_ref.clone()).or_default().insert(entry.id.clone());
                    // Rebuild the monotonic delay from the absolute wall deadline.
                    // Within a process this cancels — the rule minted `fire_at`
                    // from this same `now_ms()`, so delay == the original delay_ms
                    // regardless of the clock anchor. Across a failover `restore`,
                    // `fire_at` came from the *dead* node's clock, so the result
                    // depends on cross-node wall-clock agreement (see `restore`).
                    // Past-due (`fire_at <= now`) clamps to 0 → fires next tick.
                    let delay = (entry.fire_at - clock.now_ms()).max(0) as u64;
                    queue.insert(
                        Fired {
                            id: entry.id,
                            epoch,
                            timer_type: entry.timer_type,
                            call_ref,
                            leg_id: entry.leg_id,
                        },
                        Duration::from_millis(delay),
                    );
                }
                Some(TimerCmd::Cancel { id }) => {
                    // Logical cancel: forget the live epoch. The queued entry
                    // stays and drops as a tombstone at its deadline.
                    active.remove(&id);
                    for ids in by_call.values_mut() {
                        ids.remove(&id);
                    }
                }
                Some(TimerCmd::CancelAll { call_ref }) => {
                    if let Some(ids) = by_call.remove(&call_ref) {
                        for id in ids {
                            active.remove(&id);
                        }
                    }
                }
            },
            expired = next_expired(&mut queue), if !queue.is_empty() => {
                // Deliver only the live generation; a stale epoch (rescheduled)
                // or a missing id (cancelled) is a tombstone — drop it.
                if active.get(&expired.id) != Some(&expired.epoch) {
                    continue;
                }
                active.remove(&expired.id);
                if let Some(ids) = by_call.get_mut(&expired.call_ref) {
                    ids.remove(&expired.id);
                }
                let event = CallEvent::Timer {
                    timer_type: expired.timer_type,
                    call_ref: expired.call_ref,
                    leg_id: expired.leg_id,
                };
                // Unbounded: only fails if the router (receiver) is gone — i.e. the
                // worker is shutting down — in which case dropping the fire is fine.
                let _ = fire_tx.send(event);
            }
        }
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
        timers.cancel("t1".into()).await;
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
        timers.cancel("KeepaliveTimeout:a".into()).await;

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
}
