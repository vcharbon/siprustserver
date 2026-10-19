//! [`RefreshBatch`] — the worker's batch of limiter refreshes.
//!
//! A counted call's `LimiterRefresh` timer marks the call's key due here and
//! the call's turn goes on: no call waits on a refresh. One sender
//! ([`RefreshBatch::run`]) sends, one tick after a key fell due (the
//! configured tick, shortened to fit the lease: [`LimiterLease`]), every key
//! due, [`RefreshBatchConfig::max`] keys per request, one request at a time,
//! under the client's refresh budget, each carrying the set its call last
//! learnt it holds. Each call's answer that changes or concerns the call
//! ([`RefreshAnswered`]: every outcome but `Extended`, and an `Extended` one
//! stating another set than the one sent) is handed back to the call, which
//! applies it on its own turn.
//!
//! A key marked again while due keeps its place and its expiry, and takes the
//! latest held set. A request with no usable answer puts its keys
//! back, and the next round waits the batch's backoff: a tick, or longer once
//! the doubling from `BACKOFF_INITIAL` passes it, up to `BACKOFF_MAX`;
//! an answered request ends it. The limiter is one endpoint, so the backoff
//! is the batch's, not the entry's. Bounds: an entry due for one lease from
//! its first mark is given up (the call's own refresh marks it again every
//! period), the lease being the limiter's as the worker last learnt it
//! ([`LimiterLease`]); a mark onto a full batch gives up the oldest entry
//! waiting, and the release of a call forgets its entry, so an ended call's set is never
//! refreshed after its release was queued; all three are counted. An entry is
//! a key, its call and its held set. The batch is not replicated:
//! a worker that dies loses it, and a call a peer materialises marks itself.
//!
//! `RefreshBatch::hold` and `RefreshBatch::resume` are the seam the
//! worker's circuit breaker drives: while held nothing is sent, entries are
//! still given up at their deadline (the sender wakes on it and on each change
//! of the lease), and a resume sends every key due at once, whatever the
//! backoff.
//! The sender is supervised: one that panics is restarted with the batch
//! intact, and counted.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

use crate::abort_on_drop::AbortOnDrop;
use crate::config::B2buaConfig;
use crate::limiter::lease::LimiterLease;
use crate::limiter::release_queue::lease_changed;
use crate::limiter::{
    CallLimiter, LimiterHeld, RefreshAnswer, RefreshCall, RefreshOutcome, RefreshReply,
};
use crate::metrics::{B2buaMetrics, LimiterTask, RefreshGiveUp};
use b2bua_sdk::event::CallEvent;

/// The backoff after the first unanswered request (a tick when shorter).
const BACKOFF_INITIAL: Duration = Duration::from_millis(200);

/// The longest wait between two rounds while the limiter keeps failing.
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// The batch's pace and bounds.
#[derive(Clone)]
pub struct RefreshBatchConfig {
    /// How long the batch collects after a key fell due before it sends,
    /// under a lease it fits.
    pub tick: Duration,
    /// Most keys one request carries.
    pub max: usize,
    /// How long an entry is worth sending after its first mark: the
    /// limiter's lease as last learnt.
    pub lease: Arc<LimiterLease>,
    /// Most entries the batch holds, the request in flight included.
    pub cap: usize,
}

impl RefreshBatchConfig {
    /// The pace and bounds `config` states: its tick and batch size, and
    /// the release queue's cap; and the worker's learnt `lease`.
    pub(crate) fn from_config(config: &B2buaConfig, lease: Arc<LimiterLease>) -> Self {
        Self {
            tick: Duration::from_millis(config.limiter_refresh_batch_ms.max(1)),
            max: config.limiter_refresh_batch_max.max(1),
            lease,
            cap: config.limiter_release_queue_cap.max(1),
        }
    }
}

/// One call's refresh answer, handed back to the call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshAnswered {
    /// The call the refresh was marked for.
    pub(crate) call_ref: String,
    /// The limiter key refreshed.
    pub(crate) key: String,
    /// The limiter's answer for the key.
    pub(crate) outcome: RefreshOutcome,
    /// The set the limiter stated it holds for the key (`None` when
    /// released).
    pub(crate) held: Option<LimiterHeld>,
}

impl RefreshAnswered {
    /// The internal-event topic an answer rides to its call.
    pub(crate) const TOPIC: &'static str = "limiter-refresh";

    /// The internal event that carries this answer to its call.
    pub(crate) fn into_event(self) -> CallEvent {
        CallEvent::InternalEvent {
            call_ref: self.call_ref,
            topic: Self::TOPIC.to_string(),
            outcome: outcome_label(self.outcome).to_string(),
            incarnation: Some(self.key.clone()),
            payload: serde_json::json!({ "key": self.key, "held": self.held }),
            body: Vec::new(),
        }
    }

    /// The answer `event` carries, if it is one.
    pub(crate) fn of(event: &CallEvent) -> Option<Self> {
        let CallEvent::InternalEvent { call_ref, topic, outcome, payload, .. } = event else {
            return None;
        };
        if topic != Self::TOPIC {
            return None;
        }
        let outcome = match outcome.as_str() {
            "extended" => RefreshOutcome::Extended,
            "reregistered" => RefreshOutcome::Reregistered,
            "released" => RefreshOutcome::Released,
            "dropped" => RefreshOutcome::Dropped,
            _ => return None,
        };
        Some(Self {
            call_ref: call_ref.clone(),
            key: payload.get("key")?.as_str()?.to_string(),
            outcome,
            held: serde_json::from_value(payload.get("held")?.clone()).ok()?,
        })
    }
}

/// The metric and event label of `outcome`.
pub(crate) const fn outcome_label(outcome: RefreshOutcome) -> &'static str {
    match outcome {
        RefreshOutcome::Extended => "extended",
        RefreshOutcome::Reregistered => "reregistered",
        RefreshOutcome::Released => "released",
        RefreshOutcome::Dropped => "dropped",
    }
}

/// Where the batch hands each [`RefreshAnswered`].
type AnswerSink = Box<dyn Fn(RefreshAnswered) + Send + Sync>;

/// One key due.
struct Entry {
    call_ref: String,
    held: LimiterHeld,
    /// The mark that placed the entry: its tie-break in [`Due::order`].
    seq: u64,
    /// The key's first mark: one lease later the entry is given up.
    first_marked_at: Instant,
}

impl Entry {
    /// The entry's place in [`Due::order`].
    fn place(&self) -> (Instant, u64) {
        (self.first_marked_at, self.seq)
    }
}

#[derive(Default)]
struct Due {
    by_key: HashMap<String, Entry>,
    /// Keys by first mark, then by mark: the first expires first, and is the
    /// oldest.
    order: BTreeMap<(Instant, u64), String>,
    next_seq: u64,
    /// The entries of the request in flight, by key. A failed request puts
    /// back those still here; a forget removes its key from here too.
    in_flight: HashMap<String, Entry>,
    /// A breaker holds the batch: nothing is sent.
    held: bool,
    /// The batch resumed: the next round leaves without waiting.
    send_now: bool,
    /// Consecutive unanswered requests.
    failures: u32,
    /// The next round waits until this instant (set by an unanswered request).
    retry_at: Option<Instant>,
}

impl Due {
    fn len(&self) -> usize {
        self.by_key.len() + self.in_flight.len()
    }

    /// Hold `entry` under `key` at its own place.
    fn insert(&mut self, key: String, entry: Entry) {
        self.order.insert(entry.place(), key.clone());
        self.by_key.insert(key, entry);
    }
}

/// The worker's batch of limiter refreshes. See the module doc.
pub struct RefreshBatch {
    limiter: Arc<dyn CallLimiter>,
    config: RefreshBatchConfig,
    metrics: B2buaMetrics,
    answers: AnswerSink,
    due: Mutex<Due>,
    /// Wakes the sender: a key fell due, or the batch resumed.
    wake: Notify,
    /// Ends the sender's tick: the batch resumed.
    resumed: Notify,
}

impl RefreshBatch {
    /// An empty batch sending through `limiter` and handing every answer
    /// to `answers`. Nothing is sent until [`run`](Self::run) is spawned.
    pub fn new(
        limiter: Arc<dyn CallLimiter>,
        config: RefreshBatchConfig,
        metrics: B2buaMetrics,
        answers: impl Fn(RefreshAnswered) + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            limiter,
            config,
            metrics,
            answers: Box::new(answers),
            due: Mutex::new(Due::default()),
            wake: Notify::new(),
            resumed: Notify::new(),
        })
    }

    /// Every step leaves the state whole: a poisoned lock is taken as it is.
    fn lock(&self) -> MutexGuard<'_, Due> {
        crate::poison::locked(&self.due, crate::poison::LIMITER_REFRESH_DUE)
    }

    /// Mark the refresh of `key`, the limiter key of `call_ref`, due with
    /// the set `held` the call last learnt it holds, and return at once.
    pub(crate) fn mark(&self, key: &str, call_ref: &str, held: &LimiterHeld) {
        let now = Instant::now();
        let mut due = self.lock();
        self.expire(&mut due, now);
        if let Some(entry) = due.by_key.get_mut(key) {
            entry.call_ref = call_ref.to_string();
            entry.held = held.clone();
            return;
        }
        while due.len() >= self.config.cap {
            let Some((_, oldest)) = due.order.pop_first() else {
                // Every entry is in flight: this mark is the one given up.
                self.metrics.limiter().count_refresh_given_up(RefreshGiveUp::Cap);
                return;
            };
            due.by_key.remove(&oldest);
            self.metrics.limiter().count_refresh_given_up(RefreshGiveUp::Cap);
        }
        let seq = due.next_seq;
        due.next_seq += 1;
        // A key in flight keeps its first mark.
        let first_marked_at = due.in_flight.get(key).map_or(now, |e| e.first_marked_at);
        let entry =
            Entry { call_ref: call_ref.to_string(), held: held.clone(), seq, first_marked_at };
        due.insert(key.to_string(), entry);
        self.publish(&due);
        drop(due);
        self.wake.notify_one();
    }

    /// Forget the refresh of `key`: its call's release was queued. A request
    /// in flight carrying it is not recalled, and a failed one does not put
    /// it back.
    pub(crate) fn forget(&self, key: &str) {
        let mut due = self.lock();
        let waiting = due.by_key.remove(key).map(|entry| due.order.remove(&entry.place()));
        let in_flight = due.in_flight.remove(key);
        if waiting.is_some() || in_flight.is_some() {
            self.metrics.limiter().count_refresh_given_up(RefreshGiveUp::Released);
            self.publish(&due);
        }
    }

    /// Keys due and not given up, the request in flight included.
    pub(crate) fn due(&self) -> usize {
        let mut due = self.lock();
        self.expire(&mut due, Instant::now());
        due.len()
    }

    /// Stop sending (a breaker opened): entries wait, and still expire.
    pub(crate) fn hold(&self) {
        self.lock().held = true;
    }

    /// Send again (a breaker closed): every key due leaves at once, whatever
    /// the backoff. A resume during a round, which sends every key due anyway,
    /// lets the next round leave at once too.
    pub(crate) fn resume(&self) {
        {
            let mut due = self.lock();
            due.held = false;
            due.send_now = true;
            due.failures = 0;
            due.retry_at = None;
        }
        self.wake.notify_one();
        self.resumed.notify_one();
    }

    /// The supervised sender, until the task is aborted with the worker. A
    /// sender that panics is logged, counted and started again; what it was
    /// sending is put back, and leaves one tick later.
    pub async fn run(self: Arc<Self>) {
        loop {
            let mut sender = AbortOnDrop(observe::spawn_keeping_node(self.clone().send()));
            match (&mut sender.0).await {
                Err(e) if e.is_panic() => {
                    self.put_back();
                    self.metrics.limiter().count_task_restart(LimiterTask::RefreshSender);
                    tracing::error!(
                        due = self.due(),
                        "limiter refresh sender panicked; restarting it"
                    );
                }
                _ => return,
            }
        }
    }

    /// Wait for a key due, collect for one tick or the backoff (cut short by
    /// a resume), then send every key due; a request with no usable answer
    /// ends the round and backs off, and its keys wait for the next.
    async fn send(self: Arc<Self>) {
        let mut lease = self.config.lease.changes();
        loop {
            while !self.sendable() {
                // Nothing to send: wait for a key, a resume, the first
                // entry's deadline or a change of the lease that moves it.
                let deadline = self.first_deadline();
                tokio::select! {
                    _ = self.wake.notified() => {}
                    _ = sleep_until(deadline) => {}
                    _ = lease_changed(&mut lease) => {}
                }
            }
            self.wait_round().await;
            self.lock().send_now = false;
            while let Some(calls) = self.take() {
                match self.limiter.refresh(&calls).await {
                    RefreshAnswer::Answered(replies) => self.settle(&calls, replies),
                    RefreshAnswer::Unavailable => {
                        self.put_back();
                        self.back_off();
                        break;
                    }
                }
            }
        }
    }

    /// Wait one tick, or until the backoff ends, or less when the batch
    /// resumes.
    async fn wait_round(&self) {
        let until =
            self.lock().retry_at.unwrap_or_else(|| sip_clock::deadline::from_now(self.tick()));
        let resumed = async {
            while !self.lock().send_now {
                self.resumed.notified().await;
            }
        };
        tokio::select! {
            _ = tokio::time::sleep_until(until) => {}
            _ = resumed => {}
        }
    }

    /// One more unanswered request: no round before the next backoff step,
    /// and never before a tick.
    fn back_off(&self) {
        let mut due = self.lock();
        due.failures = due.failures.saturating_add(1);
        let wait = backoff(due.failures).max(self.tick());
        due.retry_at = Some(sip_clock::deadline::from_now(wait));
    }

    /// When the first entry waiting is given up under the current lease.
    fn first_deadline(&self) -> Option<Instant> {
        let due = self.lock();
        let kept = self.config.lease.current();
        due.order.first_key_value().map(|((first_marked_at, _), _)| *first_marked_at + kept)
    }

    /// The tick under the current lease.
    fn tick(&self) -> Duration {
        self.config.lease.refresh_tick(self.config.tick)
    }

    /// Whether a key is due and the batch not held.
    fn sendable(&self) -> bool {
        let mut due = self.lock();
        self.expire(&mut due, Instant::now());
        !due.held && !due.by_key.is_empty()
    }

    /// Up to [`RefreshBatchConfig::max`] keys due, oldest first, moved in
    /// flight; `None` when nothing is due or the batch is held.
    fn take(&self) -> Option<Vec<RefreshCall>> {
        let mut due = self.lock();
        self.expire(&mut due, Instant::now());
        if due.held || due.by_key.is_empty() {
            return None;
        }
        let mut calls = Vec::new();
        while calls.len() < self.config.max {
            let Some((_, key)) = due.order.pop_first() else { break };
            let Some(entry) = due.by_key.remove(&key) else { continue };
            calls.push(RefreshCall { key: key.clone(), held: entry.held.clone() });
            due.in_flight.insert(key, entry);
        }
        Some(calls)
    }

    /// Apply an answered request: every key leaves the batch, each answer is
    /// counted, and each one that is not `Extended`, or states another set
    /// than the one the refresh carried, is handed to its call.
    fn settle(&self, calls: &[RefreshCall], replies: Vec<RefreshReply>) {
        debug_assert_eq!(calls.len(), replies.len(), "one reply per call");
        let mut handed = Vec::new();
        {
            let mut due = self.lock();
            due.failures = 0;
            due.retry_at = None;
            for (call, RefreshReply { outcome, held }) in calls.iter().zip(replies) {
                let entry = due.in_flight.remove(&call.key);
                self.metrics.limiter().count_refresh_answer(outcome);
                if outcome == RefreshOutcome::Extended && held.as_ref() == Some(&call.held) {
                    continue;
                }
                // A forgotten key's call ended: nothing to hand back.
                if let Some(entry) = entry {
                    handed.push(RefreshAnswered {
                        call_ref: entry.call_ref,
                        key: call.key.clone(),
                        outcome,
                        held,
                    });
                }
            }
            due.in_flight.clear();
            self.publish(&due);
        }
        for answer in handed {
            (self.answers)(answer);
        }
    }

    /// Put the request in flight back: each key not forgotten waits at its
    /// own place, unless the call marked it again meanwhile.
    fn put_back(&self) {
        let mut due = self.lock();
        let in_flight = std::mem::take(&mut due.in_flight);
        let mut kept = 0;
        for (key, entry) in in_flight {
            if due.by_key.contains_key(&key) {
                continue;
            }
            due.insert(key, entry);
            kept += 1;
        }
        self.metrics.limiter().count_refresh_retries(kept);
        self.publish(&due);
    }

    /// Give up every entry due for one lease.
    fn expire(&self, due: &mut Due, now: Instant) {
        let kept = self.config.lease.current();
        let mut expired = false;
        while let Some(first) = due.order.first_entry() {
            if first.key().0 + kept > now {
                break;
            }
            let key = first.remove();
            if due.by_key.remove(&key).is_some() {
                self.metrics.limiter().count_refresh_given_up(RefreshGiveUp::LeaseExpired);
                expired = true;
            }
        }
        if expired {
            self.publish(due);
        }
    }

    fn publish(&self, due: &Due) {
        self.metrics.limiter().set_refresh_due(due.len() as u64);
    }
}

/// Sleep until `deadline`; never without one.
async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// The wait after `failures` consecutive unanswered requests, before the
/// tick floor.
fn backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    BACKOFF_INITIAL.saturating_mul(1 << doublings).min(BACKOFF_MAX)
}

#[cfg(test)]
mod tests;
