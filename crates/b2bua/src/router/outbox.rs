//! The per-call outbox (ADR-0055 §3): every effect a call's turns send off the
//! node (its SIP, its requests to outside services, its limiter releases, the
//! transactions it relinquishes) leaves through it, in the order the turns
//! queued it: a later entry never overtakes an earlier one. The replication
//! sync level decides only when an entry is released: under `off` at once,
//! under `applied` once the call's backup applied the write that records it,
//! or once the cap counted from the entry's own enqueue expired, whichever
//! comes first. An entry of a call with no live, bootstrapped backup waits for
//! nothing. No turn waits on the outbox: an entry that cannot leave at once is
//! sent by the call's drainer task, which a fence aborts with the worker's
//! in-flight turns. The entries of one call are queued under its per-call
//! lock.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{watch, Notify};
use tokio::time::Instant;

use crate::dispatch::InFlight;
use crate::metrics::B2buaMetrics;
use crate::poison::{locked, OUTBOX};
use crate::repl::{Changelog, Reach, SyncLevel, WritePath};
use crate::store::HoldTarget;
use crate::turn_probe::Released;

/// What an entry does once released, told why it was.
pub(crate) type Effects =
    Box<dyn FnOnce(Released) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

/// The outboxes of the node's calls.
pub struct Outboxes {
    level: SyncLevel,
    cap: Duration,
    changelog: Option<Changelog>,
    metrics: B2buaMetrics,
    calls: Mutex<HashMap<String, Arc<CallOutbox>>>,
    depth: AtomicUsize,
    /// The worker's in-flight tasks: a fence aborts the drainers with them.
    in_flight: InFlight,
}

/// One call's queued entries. It exists while the call has an entry that
/// could not leave at once, and its drainer runs while it exists.
struct CallOutbox {
    entries: Mutex<VecDeque<Entry>>,
    next_id: std::sync::atomic::AtomicU64,
    /// Held while an entry is sent, so a release taking the rest sends them
    /// after the one in flight.
    sending: tokio::sync::Mutex<()>,
    /// Wakes the drainer once a release took its head.
    kick: Notify,
    /// `true` once the outbox is empty and gone.
    done: watch::Sender<bool>,
}

struct Entry {
    id: u64,
    wait: Option<HoldTarget>,
    enqueued: Instant,
    effects: Effects,
}

impl Outboxes {
    /// The outboxes of a node writing on a write path into a change log
    /// (`wired`), their drainers tracked in `in_flight`; an unwired node
    /// (`None`) waits for nothing.
    pub fn new(
        wired: Option<(WritePath, Changelog)>,
        metrics: B2buaMetrics,
        in_flight: InFlight,
    ) -> Self {
        let (write_path, changelog) = match wired {
            Some((path, log)) => (path, Some(log)),
            None => (WritePath::default(), None),
        };
        Self {
            level: write_path.sync,
            cap: write_path.sync_cap,
            changelog,
            metrics,
            calls: Mutex::default(),
            depth: AtomicUsize::new(0),
            in_flight,
        }
    }

    /// Entries queued or being sent across every call.
    pub fn depth(&self) -> usize {
        self.depth.load(Ordering::Relaxed)
    }

    fn calls(&self) -> MutexGuard<'_, HashMap<String, Arc<CallOutbox>>> {
        locked(&self.calls, OUTBOX)
    }

    /// Queue `effects` of `call_ref`, waiting for `wait` (the backup applying
    /// the write that records them) under `applied`. They run here, before
    /// this returns, when the call has nothing queued and they wait for
    /// nothing; otherwise the call's drainer runs them in turn.
    pub(crate) async fn push(
        self: &Arc<Self>,
        call_ref: &str,
        wait: Option<HoldTarget>,
        effects: Effects,
    ) {
        let wait = wait.filter(|_| self.level == SyncLevel::Applied);
        let ready = match &wait {
            None => Released::AtOnce,
            Some(target) if self.applied(target) => Released::Applied,
            Some(_) => {
                self.queue(call_ref, wait, effects);
                return;
            }
        };
        // A call with entries queued keeps its order: this entry goes behind.
        let Err(effects) = self.queue_behind(call_ref, wait, effects) else { return };
        self.publish_depth(1, 0);
        effects(ready).await;
        self.publish_depth(0, 1);
    }

    /// Queue `effects` behind `call_ref`'s queued entries, when it has any.
    fn queue_behind(
        &self,
        call_ref: &str,
        wait: Option<HoldTarget>,
        effects: Effects,
    ) -> Result<(), Effects> {
        let calls = self.calls();
        let Some(outbox) = calls.get(call_ref) else { return Err(effects) };
        outbox.queue(wait, effects);
        self.publish_depth(1, 0);
        Ok(())
    }

    /// Queue `effects` on `call_ref`'s outbox, starting its drainer when the
    /// outbox is new. An outbox in the map has its drainer running: the
    /// drainer leaves the map, under the lock held here, only once the outbox
    /// is empty.
    fn queue(self: &Arc<Self>, call_ref: &str, wait: Option<HoldTarget>, effects: Effects) {
        let mut calls = self.calls();
        let (outbox, created) = match calls.get(call_ref) {
            Some(outbox) => (outbox.clone(), false),
            None => {
                let outbox = Arc::new(CallOutbox::new());
                calls.insert(call_ref.to_string(), outbox.clone());
                (outbox, true)
            }
        };
        outbox.queue(wait, effects);
        self.publish_depth(1, 0);
        drop(calls);
        if created {
            let (outboxes, call_ref) = (self.clone(), call_ref.to_string());
            let drainer =
                observe::spawn_keeping_node(async move { outboxes.drain(call_ref, outbox).await });
            self.in_flight.track(drainer.abort_handle());
        }
    }

    /// Run now, in order, every entry `call_ref` still has queued: the call is
    /// released here, and its copy's write records them.
    pub(crate) async fn release(&self, call_ref: &str) {
        let Some(outbox) = self.calls().get(call_ref).cloned() else { return };
        let _sending = outbox.sending.lock().await;
        while let Some(entry) = outbox.pop_front() {
            (entry.effects)(Released::CallReleased).await;
            self.publish_depth(0, 1);
        }
        outbox.kick.notify_one();
    }

    /// Resolves once `call_ref` has nothing queued: at most the cap after its
    /// last entry was queued.
    pub(crate) async fn drained(&self, call_ref: &str) {
        let Some(outbox) = self.calls().get(call_ref).cloned() else { return };
        let mut done = outbox.done.subscribe();
        let _ = done.wait_for(|done| *done).await;
    }

    /// Send `call_ref`'s entries in turn, each once released, until none is
    /// left; the outbox then goes, under the map lock a push queues under. A
    /// drainer that ends otherwise (a panicking effect, a fence) takes its
    /// outbox with it, so no later entry queues behind a dead drainer.
    async fn drain(self: Arc<Self>, call_ref: String, outbox: Arc<CallOutbox>) {
        let _exit = DrainerExit { outboxes: &self, call_ref: &call_ref, outbox: &outbox };
        loop {
            let head = outbox.entries().front().map(|e| (e.id, e.wait.clone(), e.enqueued));
            let Some((id, wait, enqueued)) = head else {
                let calls = self.calls();
                if outbox.entries().is_empty() {
                    self.forget(calls, &call_ref, &outbox);
                    return;
                }
                continue;
            };
            let released = match &wait {
                None => Some(Released::AtOnce),
                Some(target) => self.wait(&call_ref, target, enqueued, &outbox.kick).await,
            };
            let Some(released) = released else { continue };
            let _sending = outbox.sending.lock().await;
            let entry = {
                let mut entries = outbox.entries();
                match entries.front() {
                    Some(head) if head.id == id => entries.pop_front(),
                    _ => None,
                }
            };
            let Some(entry) = entry else { continue };
            if released == Released::Capped {
                self.metrics.bump_repl_sync_capped();
            }
            (entry.effects)(released).await;
            self.publish_depth(0, 1);
        }
    }

    /// Remove `outbox` of `call_ref` from the map `calls` guards, dropping
    /// what it still holds, and mark it done.
    fn forget(
        &self,
        mut calls: MutexGuard<'_, HashMap<String, Arc<CallOutbox>>>,
        call_ref: &str,
        outbox: &Arc<CallOutbox>,
    ) {
        if calls.get(call_ref).is_some_and(|o| Arc::ptr_eq(o, outbox)) {
            calls.remove(call_ref);
        }
        let dropped = std::mem::take(&mut *outbox.entries()).len();
        if dropped > 0 {
            self.publish_depth(0, dropped);
        }
        outbox.done.send_replace(true);
    }

    /// Wait until the entry of `call_ref` queued at `enqueued` waiting for
    /// `target` is released; `None` when `kick` woke the drainer first. A
    /// later write of the call moves its change-log entry, and the wait goes
    /// on to that write, which records this entry too; the cap stays counted
    /// from this entry's enqueue.
    async fn wait(
        &self,
        call_ref: &str,
        target: &HoldTarget,
        enqueued: Instant,
        kick: &Notify,
    ) -> Option<Released> {
        let Some(log) = &self.changelog else { return Some(Released::AtOnce) };
        let deadline = enqueued + self.cap;
        let current = |at| {
            let now = log.position_of(&target.peer, target.partition, call_ref);
            now.map_or(at, |now| std::cmp::max(at, now))
        };
        let mut at = current(target.at);
        loop {
            tokio::select! {
                biased;
                reach = log.applied_reaches(&target.peer, target.partition, at) => match reach {
                    Reach::Lost => return Some(Released::BackupLost),
                    Reach::Applied => {
                        let moved = current(at);
                        if moved == at {
                            return Some(Released::Applied);
                        }
                        at = moved;
                    }
                },
                () = tokio::time::sleep_until(deadline) => return Some(Released::Capped),
                () = kick.notified() => return None,
            }
        }
    }

    /// Whether `target` is applied already.
    fn applied(&self, target: &HoldTarget) -> bool {
        self.changelog
            .as_ref()
            .is_some_and(|log| log.has_applied(&target.peer, target.partition, target.at))
    }

    /// Count `queued` entries in and `left` out, and publish the depth.
    fn publish_depth(&self, queued: usize, left: usize) {
        let depth = if queued > 0 {
            self.depth.fetch_add(queued, Ordering::Relaxed) + queued
        } else {
            self.depth.fetch_sub(left, Ordering::Relaxed) - left
        };
        self.metrics.set_repl_outbox_depth(depth as u64);
    }
}

/// Takes a drainer's outbox out of the map however the drainer ends.
struct DrainerExit<'a> {
    outboxes: &'a Outboxes,
    call_ref: &'a str,
    outbox: &'a Arc<CallOutbox>,
}

impl Drop for DrainerExit<'_> {
    fn drop(&mut self) {
        if !*self.outbox.done.borrow() {
            self.outboxes.forget(self.outboxes.calls(), self.call_ref, self.outbox);
        }
    }
}

impl CallOutbox {
    fn new() -> Self {
        Self {
            entries: Mutex::default(),
            next_id: std::sync::atomic::AtomicU64::new(0),
            sending: tokio::sync::Mutex::new(()),
            kick: Notify::new(),
            done: watch::Sender::new(false),
        }
    }

    fn entries(&self) -> MutexGuard<'_, VecDeque<Entry>> {
        locked(&self.entries, OUTBOX)
    }

    /// Queue `effects` behind every entry queued before, waiting for `wait`.
    fn queue(&self, wait: Option<HoldTarget>, effects: Effects) {
        let mut entries = self.entries();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        entries.push_back(Entry { id, wait, enqueued: Instant::now(), effects });
    }

    fn pop_front(&self) -> Option<Entry> {
        self.entries().pop_front()
    }
}

#[cfg(test)]
#[path = "outbox_tests.rs"]
mod tests;
