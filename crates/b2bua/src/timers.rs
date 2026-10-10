//! `TimerService` — one `tokio_util::time::DelayQueue` driver holding every
//! pending B2BUA timer (keepalive, no-answer, max-duration, terminating-safety,
//! …). Port of `TimerService.ts`, reusing the single-`DelayQueue` shape from
//! sip-txn (ADR-0007) but with a B2BUA-local driver (sip-txn's queue is private
//! to its actor). Rides `tokio::time`, so `pause`/`advance` drives it in tests.
//!
//! On expiry the driver emits a [`CallEvent::Timer`] on its fire channel; the
//! router selects on that channel and routes it through the per-call dispatcher
//! exactly like an inbound message — so timer handling shares the per-call FIFO.
//! The fire carries the incarnation of the call that scheduled it, so a fire
//! already on the channel when that call ended reaches no later call on its
//! `call_ref` (the router drops it).
//!
//! ## Cancellation: epoch correctness **+** physical `Key` removal
//!
//! A `DelayQueue` `Key` is a bare slab index with no generation stamp: when an
//! entry expires or is removed, its slot is freed and the **next insert reuses
//! it, yielding the same `Key` value**. A *stale* `id → Key` map therefore
//! aliases, and `try_remove(stale_key)` then evicts whatever *live* timer now
//! occupies that slot — a silent, catastrophic wrong-timer cancel (e.g. a
//! keepalive-timeout cancel killing the rescheduled keepalive). See
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
//!   timer (default 1 h) cancelled by a 30 s call's BYE lingers ~3570 s as a
//!   tombstone; under steady load the queue grows to ≈ `arrival_rate × 3600`
//!   (~850k entries at ~100 cps) and the oversized timing wheel drives a
//!   monotonic CPU climb that looks like a leak but isn't. Physical removal
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

use crate::metrics::B2buaMetrics;
use b2bua_sdk::event::CallEvent;

struct Fired {
    id: String,
    epoch: u64,
    timer_type: TimerType,
    call_ref: String,
    leg_id: Option<String>,
    incarnation: Option<String>,
}

enum TimerCmd {
    Schedule { entry: TimerEntry, call_ref: String, incarnation: Option<String> },
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

    /// Arm `entry` for the call `call_ref` whose incarnation is
    /// `incarnation` (`call::Call::incarnation`), which its fire carries.
    pub async fn schedule(&self, entry: TimerEntry, call_ref: String, incarnation: Option<String>) {
        let _ = self.cmd_tx.send(TimerCmd::Schedule { entry, call_ref, incarnation }).await;
    }

    pub async fn cancel(&self, call_ref: String, id: String) {
        let _ = self.cmd_tx.send(TimerCmd::Cancel { call_ref, id }).await;
    }

    pub async fn cancel_all(&self, call_ref: String) {
        let _ = self.cmd_tx.send(TimerCmd::CancelAll { call_ref }).await;
    }

    /// Re-arm a hydrated call's persisted timer intents into this node's driver.
    ///
    /// Called once, on the materialisation of a call from a replica
    /// (`router::materialise`, on the pass that inserted the copy).
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
    pub async fn restore(&self, entries: Vec<TimerEntry>, call_ref: String, incarnation: &str) {
        for entry in entries {
            self.schedule(entry, call_ref.clone(), Some(incarnation.to_string())).await;
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
                Some(TimerCmd::Schedule { entry, call_ref, incarnation }) => {
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
                            incarnation,
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
                        incarnation: expired.incarnation,
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
        // cancellation the two track each other — their gap is the tombstone
        // backlog, which should stay ≈ 0. A gap that *climbs* means a removal is
        // being missed and entries linger (e.g. a cancelled 1 h GlobalDuration
        // left to expire); it is the alarm for that leak. Updated
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
mod tests;
