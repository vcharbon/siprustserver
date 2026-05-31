//! `TimerService` — one `tokio_util::time::DelayQueue` driver holding every
//! pending B2BUA timer (keepalive, no-answer, max-duration, terminating-safety,
//! …). Port of `TimerService.ts`, reusing the single-`DelayQueue` shape from
//! sip-txn (ADR-0007) but with a B2BUA-local driver (sip-txn's queue is private
//! to its actor). Rides `tokio::time`, so `pause`/`advance` drives it in tests.
//!
//! On expiry the driver emits a [`CallEvent::Timer`] on its fire channel; the
//! router selects on that channel and routes it through the per-call dispatcher
//! exactly like an inbound message — so timer handling shares the per-call FIFO.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use sip_clock::Clock;
use tokio::sync::mpsc;
use tokio_util::time::{delay_queue::Key, DelayQueue};

use call::{TimerEntry, TimerType};

use crate::event::CallEvent;

struct Fired {
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
    pub fn spawn(clock: Clock) -> (Self, mpsc::Receiver<CallEvent>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(1024);
        let (fire_tx, fire_rx) = mpsc::channel(1024);
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

    /// Re-arm persisted timer entries (crash recovery). Past-due entries fire
    /// immediately.
    pub async fn restore(&self, entries: Vec<TimerEntry>, call_ref: String) {
        for entry in entries {
            self.schedule(entry, call_ref.clone()).await;
        }
    }
}

async fn driver(
    clock: Clock,
    mut cmd_rx: mpsc::Receiver<TimerCmd>,
    fire_tx: mpsc::Sender<CallEvent>,
) {
    let mut queue: DelayQueue<Fired> = DelayQueue::new();
    let mut keys: HashMap<String, Key> = HashMap::new();
    let mut by_call: HashMap<String, HashSet<String>> = HashMap::new();

    loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => match cmd {
                None => break, // all handles dropped
                Some(TimerCmd::Schedule { entry, call_ref }) => {
                    // Replace any existing timer with the same id (idempotent re-arm).
                    if let Some(old) = keys.remove(&entry.id) {
                        queue.try_remove(&old);
                    }
                    let delay = (entry.fire_at - clock.now_ms()).max(0) as u64;
                    let key = queue.insert(
                        Fired {
                            timer_type: entry.timer_type,
                            call_ref: call_ref.clone(),
                            leg_id: entry.leg_id.clone(),
                        },
                        Duration::from_millis(delay),
                    );
                    keys.insert(entry.id.clone(), key);
                    by_call.entry(call_ref).or_default().insert(entry.id);
                }
                Some(TimerCmd::Cancel { id }) => {
                    if let Some(key) = keys.remove(&id) {
                        queue.try_remove(&key);
                    }
                }
                Some(TimerCmd::CancelAll { call_ref }) => {
                    if let Some(ids) = by_call.remove(&call_ref) {
                        for id in ids {
                            if let Some(key) = keys.remove(&id) {
                                queue.try_remove(&key);
                            }
                        }
                    }
                }
            },
            expired = next_expired(&mut queue), if !queue.is_empty() => {
                // Drop bookkeeping for the fired id (best-effort: id not tracked back
                // from the entry, so we let CancelAll/Cancel prune lazily).
                let event = CallEvent::Timer {
                    timer_type: expired.timer_type,
                    call_ref: expired.call_ref,
                    leg_id: expired.leg_id,
                };
                let _ = fire_tx.try_send(event);
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
}
