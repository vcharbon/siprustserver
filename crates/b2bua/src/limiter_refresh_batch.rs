//! [`RefreshBatch`] — the worker's batch of limiter refreshes.
//!
//! A counted call's `LimiterRefresh` timer marks the call's key due here and
//! the call's turn goes on: no call waits on a refresh. One sender
//! ([`RefreshBatch::run`]) sends, one tick after a key fell due, every key
//! due, [`RefreshBatchConfig::max`] keys per request, one request at a time,
//! under the client's refresh budget. Each call's answer that changes or
//! concerns the call ([`RefreshAnswered`]: every outcome but `Extended`) is
//! handed back to the call, which applies it on its own turn.
//!
//! A key marked again while due keeps its place and its expiry, and takes the
//! latest ids and generation. A request with no usable answer puts its keys
//! back, and the next round waits the batch's backoff: a tick, or longer once
//! the doubling from [`BACKOFF_INITIAL`] passes it, up to [`BACKOFF_MAX`];
//! an answered request ends it. The limiter is one endpoint, so the backoff
//! is the batch's, not the entry's. Bounds: an entry due for one lease from
//! its first mark (never less than two ticks, so it gets its round) is given
//! up (the call's own refresh marks it again every period), the lease being the limiter's as the worker last learnt it
//! ([`LimiterLease`]); a mark onto a full batch gives up the oldest entry
//! waiting, and
//! the release of a call forgets its entry, so an ended call's set is never
//! refreshed after its release was queued; all three are counted. An entry is
//! a key, its call, its ids and its generation. The batch is not replicated:
//! a worker that dies loses it, and a call a peer materialises marks itself.
//!
//! [`RefreshBatch::hold`] and [`RefreshBatch::resume`] are the seam the
//! worker's circuit breaker drives: while held nothing is sent, entries are
//! still given up at their deadline (the sender wakes on it and on each change
//! of the lease), and a resume sends every key due at once, whatever the
//! backoff.
//! The sender is supervised: one that panics is restarted with the batch
//! intact, and counted.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

use crate::abort_on_drop::AbortOnDrop;
use crate::config::B2buaConfig;
use crate::event::CallEvent;
use crate::limiter::{CallLimiter, RefreshAnswer, RefreshCall, RefreshOutcome};
use crate::limiter_lease::LimiterLease;
use crate::limiter_release::lease_changed;
use crate::metrics::B2buaMetrics;

/// The backoff after the first unanswered request (a tick when shorter).
pub const BACKOFF_INITIAL: Duration = Duration::from_millis(200);

/// The longest wait between two rounds while the limiter keeps failing.
pub const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// The batch's pace and bounds.
#[derive(Clone)]
pub struct RefreshBatchConfig {
    /// How long the batch collects after a key fell due before it sends.
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
    pub fn from_config(config: &B2buaConfig, lease: Arc<LimiterLease>) -> Self {
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
    pub call_ref: String,
    /// The limiter key refreshed.
    pub key: String,
    /// The generation of the call's limiter state the refresh was marked
    /// under.
    pub generation: u32,
    /// The limiter's answer for the key.
    pub outcome: RefreshOutcome,
}

impl RefreshAnswered {
    /// The internal-event topic an answer rides to its call.
    pub const TOPIC: &'static str = "limiter-refresh";

    /// The internal event that carries this answer to its call.
    pub fn into_event(self) -> CallEvent {
        CallEvent::InternalEvent {
            call_ref: self.call_ref,
            topic: Self::TOPIC.to_string(),
            outcome: outcome_label(self.outcome).to_string(),
            payload: serde_json::json!({ "key": self.key, "generation": self.generation }),
            body: Vec::new(),
        }
    }

    /// The answer `event` carries, if it is one.
    pub fn of(event: &CallEvent) -> Option<Self> {
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
            generation: u32::try_from(payload.get("generation")?.as_u64()?).ok()?,
            outcome,
        })
    }
}

/// The metric and event label of `outcome`.
pub fn outcome_label(outcome: RefreshOutcome) -> &'static str {
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
    ids: Vec<String>,
    generation: u32,
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

    /// Every step leaves the state whole, so a poisoned lock is taken as it
    /// is.
    fn lock(&self) -> MutexGuard<'_, Due> {
        self.due.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Mark the refresh of `key`, the limiter key of `call_ref`, due with
    /// `ids` under the call's limiter `generation`, and return at once.
    pub fn mark(&self, key: &str, call_ref: &str, ids: &[String], generation: u32) {
        let now = Instant::now();
        let mut due = self.lock();
        self.expire(&mut due, now);
        if let Some(entry) = due.by_key.get_mut(key) {
            entry.call_ref = call_ref.to_string();
            entry.ids = ids.to_vec();
            entry.generation = generation;
            return;
        }
        while due.len() >= self.config.cap {
            let Some((_, oldest)) = due.order.pop_first() else {
                // Every entry is in flight: this mark is the one given up.
                self.metrics.bump_limiter_refresh_forgotten_cap();
                return;
            };
            due.by_key.remove(&oldest);
            self.metrics.bump_limiter_refresh_forgotten_cap();
        }
        let seq = due.next_seq;
        due.next_seq += 1;
        // A key in flight keeps its first mark.
        let first_marked_at = due.in_flight.get(key).map_or(now, |e| e.first_marked_at);
        let entry = Entry {
            call_ref: call_ref.to_string(),
            ids: ids.to_vec(),
            generation,
            seq,
            first_marked_at,
        };
        due.insert(key.to_string(), entry);
        self.publish(&due);
        drop(due);
        self.wake.notify_one();
    }

    /// Forget the refresh of `key`: its call's release was queued. A request
    /// in flight carrying it is not recalled, and a failed one does not put
    /// it back.
    pub fn forget(&self, key: &str) {
        let mut due = self.lock();
        let waiting = due.by_key.remove(key).map(|entry| due.order.remove(&entry.place()));
        let in_flight = due.in_flight.remove(key);
        if waiting.is_some() || in_flight.is_some() {
            self.metrics.bump_limiter_refresh_forgotten_released();
            self.publish(&due);
        }
    }

    /// Keys due and not given up, the request in flight included.
    pub fn due(&self) -> usize {
        let mut due = self.lock();
        self.expire(&mut due, Instant::now());
        due.len()
    }

    /// Stop sending (a breaker opened): entries wait, and still expire.
    pub fn hold(&self) {
        self.lock().held = true;
    }

    /// Send again (a breaker closed): every key due leaves at once, whatever
    /// the backoff. A resume during a round, which sends every key due anyway,
    /// lets the next round leave at once too.
    pub fn resume(&self) {
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
            let mut sender = AbortOnDrop(tokio::spawn(self.clone().send()));
            match (&mut sender.0).await {
                Err(e) if e.is_panic() => {
                    self.put_back();
                    self.metrics.bump_limiter_refresh_sender_restarts();
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
                let answer = self.limiter.refresh(&calls).await;
                let answered = matches!(answer, RefreshAnswer::Answered(_));
                self.metrics.record_limiter_refresh_request(calls.len(), answered);
                match answer {
                    RefreshAnswer::Answered(outcomes) => self.settle(&calls, outcomes),
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
        let until = self.lock().retry_at.unwrap_or_else(|| Instant::now() + self.config.tick);
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
        let wait = backoff(due.failures).max(self.config.tick);
        due.retry_at = Some(Instant::now() + wait);
    }

    /// When the first entry waiting is given up under the current lease.
    fn first_deadline(&self) -> Option<Instant> {
        let due = self.lock();
        let kept = self.kept_for();
        due.order.first_key_value().map(|((first_marked_at, _), _)| *first_marked_at + kept)
    }

    /// How long an entry is kept after its first mark: one lease, and never
    /// less than two ticks, so an entry gets its round under a lease shorter
    /// than a tick.
    fn kept_for(&self) -> Duration {
        self.config.lease.current().max(self.config.tick * 2)
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
            calls.push(RefreshCall { key: key.clone(), ids: entry.ids.clone() });
            due.in_flight.insert(key, entry);
        }
        Some(calls)
    }

    /// Apply an answered request: every key leaves the batch, and each
    /// answer that is not `Extended` is counted and handed to its call.
    fn settle(&self, calls: &[RefreshCall], outcomes: Vec<RefreshOutcome>) {
        debug_assert_eq!(calls.len(), outcomes.len(), "one outcome per call");
        let mut handed = Vec::new();
        {
            let mut due = self.lock();
            due.failures = 0;
            due.retry_at = None;
            for (call, outcome) in calls.iter().zip(outcomes) {
                let entry = due.in_flight.remove(&call.key);
                match outcome {
                    RefreshOutcome::Extended => continue,
                    RefreshOutcome::Reregistered => {
                        self.metrics.bump_limiter_refresh_reregistered()
                    }
                    RefreshOutcome::Released => self.metrics.bump_limiter_refresh_released(),
                    RefreshOutcome::Dropped => self.metrics.bump_limiter_refresh_dropped(),
                }
                // A forgotten key's call ended: nothing to hand back.
                if let Some(entry) = entry {
                    handed.push(RefreshAnswered {
                        call_ref: entry.call_ref,
                        key: call.key.clone(),
                        generation: entry.generation,
                        outcome,
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
        self.metrics.add_limiter_refresh_retries(kept);
        self.publish(&due);
    }

    /// Give up every entry due for one lease (at least two ticks).
    fn expire(&self, due: &mut Due, now: Instant) {
        let kept = self.kept_for();
        let mut expired = false;
        while let Some(first) = due.order.first_entry() {
            if first.key().0 + kept > now {
                break;
            }
            let key = first.remove();
            if due.by_key.remove(&key).is_some() {
                self.metrics.bump_limiter_refresh_forgotten_lease_expired();
                expired = true;
            }
        }
        if expired {
            self.publish(due);
        }
    }

    fn publish(&self, due: &Due) {
        self.metrics.set_limiter_refresh_due(due.len() as u64);
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
mod tests {
    use std::collections::VecDeque;

    use async_trait::async_trait;
    use sip_clock::testkit::settle;

    use super::*;
    use crate::limiter::{AdmitOutcome, LimiterEntry, ReleaseAnswer};

    const TICK: Duration = Duration::from_secs(1);

    /// A limiter answering refreshes from a script of request answers (then
    /// every key `Extended`), logging every request.
    #[derive(Default)]
    struct Scripted {
        script: Mutex<VecDeque<Script>>,
        sent: Mutex<Vec<Vec<RefreshCall>>>,
        go: Notify,
    }

    enum Script {
        Answer(Vec<RefreshOutcome>),
        Unavailable,
        /// Wait for the test's go, then answer every key `Extended`, or
        /// nothing usable when `fail`.
        Wait {
            fail: bool,
        },
        Panic,
    }

    #[async_trait]
    impl CallLimiter for Scripted {
        async fn admit(&self, _: &str, _: &[LimiterEntry], _: bool) -> AdmitOutcome {
            AdmitOutcome::NotSent
        }
        async fn release(&self, _: &[String]) -> ReleaseAnswer {
            ReleaseAnswer::Released
        }
        async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer {
            self.sent.lock().unwrap().push(calls.to_vec());
            let next = self.script.lock().unwrap().pop_front();
            match next {
                Some(Script::Answer(outcomes)) => RefreshAnswer::Answered(outcomes),
                Some(Script::Unavailable) => RefreshAnswer::Unavailable,
                Some(Script::Wait { fail }) => {
                    self.go.notified().await;
                    if fail {
                        return RefreshAnswer::Unavailable;
                    }
                    RefreshAnswer::Answered(vec![RefreshOutcome::Extended; calls.len()])
                }
                Some(Script::Panic) => panic!("the limiter client panics"),
                None => RefreshAnswer::Answered(vec![RefreshOutcome::Extended; calls.len()]),
            }
        }
        fn report_to(&self, _: crate::limiter::LimiterReports) {}
    }

    struct Rig {
        limiter: Arc<Scripted>,
        batch: Arc<RefreshBatch>,
        answers: Arc<Mutex<Vec<RefreshAnswered>>>,
        metrics: B2buaMetrics,
    }

    fn rig(script: Vec<Script>, max: usize, cap: usize) -> Rig {
        rig_leased(script, max, cap, LimiterLease::starting_at(Duration::from_secs(20)))
    }

    fn rig_leased(script: Vec<Script>, max: usize, cap: usize, lease: Arc<LimiterLease>) -> Rig {
        let limiter = Arc::new(Scripted::default());
        *limiter.script.lock().unwrap() = script.into();
        let metrics = B2buaMetrics::new();
        let answers = Arc::new(Mutex::new(Vec::new()));
        let sink = answers.clone();
        let config = RefreshBatchConfig { tick: TICK, max, lease, cap };
        let batch = RefreshBatch::new(limiter.clone(), config, metrics.clone(), move |a| {
            sink.lock().unwrap().push(a)
        });
        tokio::spawn(batch.clone().run());
        Rig { limiter, batch, answers, metrics }
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Mark `key` due for call `c-<key>` with ids `[x]` at generation 0.
    fn mark(batch: &RefreshBatch, key: &str) {
        batch.mark(key, &format!("c-{key}"), &ids(&["x"]), 0);
    }

    async fn advance(d: Duration) {
        settle().await;
        tokio::time::advance(d).await;
        settle().await;
    }

    impl Rig {
        fn requests(&self) -> Vec<Vec<String>> {
            let sent = self.limiter.sent.lock().unwrap();
            sent.iter().map(|calls| calls.iter().map(|c| c.key.clone()).collect()).collect()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn every_key_due_within_a_tick_leaves_in_one_request() {
        let r = rig(Vec::new(), 100, 100);
        for key in ["a", "b", "c"] {
            mark(&r.batch, key);
            advance(Duration::from_millis(100)).await;
        }
        assert!(r.requests().is_empty(), "nothing leaves before one tick");
        advance(TICK).await;
        assert_eq!(r.requests(), [ids(&["a", "b", "c"])]);
        assert_eq!(r.batch.due(), 0);
        assert_eq!(r.metrics.limiter_refresh_requests_answered_total(), 1);
        assert_eq!(r.metrics.limiter_refresh_keys_sent_total(), 3);
        assert!(r.answers.lock().unwrap().is_empty(), "an extended answer is not handed back");
    }

    #[tokio::test(start_paused = true)]
    async fn a_key_marked_again_while_due_is_sent_once_with_its_latest_ids() {
        let r = rig(Vec::new(), 100, 100);
        r.batch.mark("a", "c-a", &ids(&["x"]), 0);
        r.batch.mark("a", "c-a", &ids(&["x", "y"]), 1);
        advance(TICK).await;
        let sent = r.limiter.sent.lock().unwrap().clone();
        assert_eq!(sent, [vec![RefreshCall { key: "a".into(), ids: ids(&["x", "y"]) }]]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_tick_sends_one_request_per_max_keys() {
        let r = rig(Vec::new(), 2, 100);
        for key in ["a", "b", "c", "d", "e"] {
            mark(&r.batch, key);
        }
        advance(TICK).await;
        assert_eq!(r.requests(), [ids(&["a", "b"]), ids(&["c", "d"]), ids(&["e"])]);
    }

    #[tokio::test(start_paused = true)]
    async fn every_answer_but_extended_is_counted_and_handed_to_its_call() {
        let script = vec![Script::Answer(vec![
            RefreshOutcome::Extended,
            RefreshOutcome::Reregistered,
            RefreshOutcome::Released,
            RefreshOutcome::Dropped,
        ])];
        let r = rig(script, 100, 100);
        for (n, key) in ["a", "b", "c", "d"].iter().enumerate() {
            r.batch.mark(key, &format!("c-{key}"), &ids(&["x"]), n as u32);
        }
        advance(TICK).await;
        let answers = r.answers.lock().unwrap().clone();
        let expected = [
            ("b", 1, RefreshOutcome::Reregistered),
            ("c", 2, RefreshOutcome::Released),
            ("d", 3, RefreshOutcome::Dropped),
        ]
        .map(|(key, generation, outcome)| RefreshAnswered {
            call_ref: format!("c-{key}"),
            key: key.to_string(),
            generation,
            outcome,
        });
        assert_eq!(answers, expected);
        assert_eq!(r.metrics.limiter_refresh_reregistered_total(), 1);
        assert_eq!(r.metrics.limiter_refresh_released_total(), 1);
        assert_eq!(r.metrics.limiter_refresh_dropped_total(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_travels_to_its_call_as_an_internal_event() {
        let answer = RefreshAnswered {
            call_ref: "c".into(),
            key: "c#k".into(),
            generation: 7,
            outcome: RefreshOutcome::Dropped,
        };
        assert_eq!(RefreshAnswered::of(&answer.clone().into_event()), Some(answer));
        let other = CallEvent::Timer {
            timer_type: call::TimerType::LimiterRefresh,
            call_ref: "c".into(),
            leg_id: None,
        };
        assert_eq!(RefreshAnswered::of(&other), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_with_no_answer_is_sent_again_at_the_next_tick() {
        let r = rig(vec![Script::Unavailable, Script::Unavailable], 2, 100);
        for key in ["a", "b", "c"] {
            mark(&r.batch, key);
        }
        advance(TICK).await;
        assert_eq!(r.requests(), [ids(&["a", "b"])], "the round ends on a failed request");
        assert_eq!(r.batch.due(), 3);
        assert_eq!(r.metrics.limiter_refresh_retries_total(), 2);
        advance(TICK).await;
        assert_eq!(r.requests()[1], ids(&["a", "b"]), "the oldest keys first, again");
        advance(TICK).await;
        assert_eq!(r.requests()[2..], [ids(&["a", "b"]), ids(&["c"])]);
        assert_eq!(r.batch.due(), 0);
        assert_eq!(r.metrics.limiter_refresh_requests_unavailable_total(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_forgotten_key_is_not_sent_nor_put_back_nor_handed_back() {
        let r = rig(vec![Script::Wait { fail: false }], 100, 100);
        mark(&r.batch, "waiting");
        mark(&r.batch, "in-flight");
        r.batch.forget("waiting");
        advance(TICK).await;
        assert_eq!(r.requests(), [ids(&["in-flight"])]);
        r.batch.forget("in-flight");
        assert_eq!(r.metrics.limiter_refresh_forgotten_released_total(), 2);
        r.batch.forget("never-marked");
        assert_eq!(r.metrics.limiter_refresh_forgotten_released_total(), 2);
        r.limiter.go.notify_one();
        advance(TICK).await;
        assert_eq!(r.batch.due(), 0);
        assert_eq!(r.requests().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_forgotten_key_in_a_failed_request_is_not_put_back() {
        let r = rig(vec![Script::Wait { fail: true }], 100, 100);
        mark(&r.batch, "a");
        mark(&r.batch, "b");
        advance(TICK).await;
        assert_eq!(r.requests(), [ids(&["a", "b"])]);
        r.batch.forget("a");
        r.limiter.go.notify_one();
        settle().await;
        assert_eq!(r.batch.due(), 1, "only b waits for the next tick");
        assert_eq!(r.metrics.limiter_refresh_retries_total(), 1);
        advance(TICK).await;
        assert_eq!(r.requests()[1], ids(&["b"]));
    }

    #[tokio::test(start_paused = true)]
    async fn a_key_marked_again_while_in_flight_keeps_its_new_mark_when_the_request_fails() {
        let r = rig(vec![Script::Wait { fail: true }], 100, 100);
        r.batch.mark("a", "c-a", &ids(&["x"]), 0);
        advance(TICK).await;
        r.batch.mark("a", "c-a", &ids(&["x", "y"]), 1);
        r.limiter.go.notify_one();
        settle().await;
        assert_eq!(r.batch.due(), 1);
        advance(TICK).await;
        let sent = r.limiter.sent.lock().unwrap().clone();
        assert_eq!(sent[1], [RefreshCall { key: "a".into(), ids: ids(&["x", "y"]) }]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_entry_due_for_one_lease_is_given_up_and_a_full_batch_gives_up_its_oldest() {
        let r = rig(Vec::new(), 100, 2);
        r.batch.hold();
        mark(&r.batch, "a");
        mark(&r.batch, "b");
        mark(&r.batch, "c");
        assert_eq!(r.batch.due(), 2);
        assert_eq!(r.metrics.limiter_refresh_forgotten_cap_total(), 1);
        advance(Duration::from_secs(21)).await;
        assert_eq!(r.batch.due(), 0, "given up one lease after the first mark");
        assert_eq!(r.metrics.limiter_refresh_forgotten_lease_expired_total(), 2);
        r.batch.resume();
        advance(TICK).await;
        assert!(r.requests().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_batch_sends_nothing_and_a_resume_sends_every_key_at_once() {
        let r = rig(Vec::new(), 100, 100);
        r.batch.hold();
        mark(&r.batch, "a");
        advance(10 * TICK).await;
        assert!(r.requests().is_empty(), "held: nothing sent");
        assert_eq!(r.metrics.limiter_refresh_due(), 1);
        mark(&r.batch, "b");
        r.batch.resume();
        advance(Duration::from_millis(1)).await;
        assert_eq!(r.requests(), [ids(&["a", "b"])], "sent at the resume, not one tick later");
    }

    #[tokio::test(start_paused = true)]
    async fn a_sender_that_panics_is_restarted_with_the_batch_intact() {
        let r = rig(vec![Script::Panic], 100, 100);
        mark(&r.batch, "a");
        advance(TICK).await;
        assert_eq!(r.metrics.limiter_refresh_sender_restarts_total(), 1);
        assert_eq!(r.batch.due(), 1, "the key the panicking request carried is put back");
        advance(TICK).await;
        assert_eq!(r.requests(), [ids(&["a"]), ids(&["a"])]);
        assert_eq!(r.batch.due(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn unanswered_requests_back_off_and_an_answer_ends_the_backoff() {
        let script = (0..9).map(|_| Script::Unavailable).collect();
        let r = rig_leased(script, 100, 100, LimiterLease::starting_at(Duration::from_secs(120)));
        let start = Instant::now();
        mark(&r.batch, "a");
        let at = |secs: f64| start + Duration::from_secs_f64(secs);
        // Sends at 1 s, then retries at 2, 3, 4 (the tick floor), 5.6, 8.8,
        // 13.8, 18.8, 23.8 (5 s at most) and 28.8 s, which is answered.
        for (until, sent) in [(1.1, 1), (4.1, 4), (5.5, 4), (5.7, 5), (13.7, 6), (13.9, 7)] {
            tokio::time::sleep_until(at(until)).await;
            settle().await;
            assert_eq!(r.requests().len(), sent, "requests by {until} s");
        }
        tokio::time::sleep_until(at(28.9)).await;
        settle().await;
        assert_eq!(r.requests().len(), 10);
        assert_eq!(r.metrics.limiter_refresh_requests_answered_total(), 1);
        assert_eq!(r.batch.due(), 0);
        mark(&r.batch, "b");
        advance(TICK + Duration::from_millis(10)).await;
        assert_eq!(r.requests().len(), 11, "answered: the next key leaves one tick later");
    }

    #[tokio::test(start_paused = true)]
    async fn a_resume_ends_the_backoff() {
        let script = (0..12).map(|_| Script::Unavailable).collect();
        let r = rig_leased(script, 100, 100, LimiterLease::starting_at(Duration::from_secs(120)));
        mark(&r.batch, "a");
        for _ in 0..200 {
            advance(TICK / 10).await;
        }
        // Sent at 1, 2, 3, 4, 5.6, 8.8, 13.8 and 18.8 s; the next waits 5 s.
        let sent = r.requests().len();
        assert_eq!(sent, 8, "backing off");
        r.batch.hold();
        r.batch.resume();
        advance(Duration::from_millis(1)).await;
        assert_eq!(r.requests().len(), sent + 1, "a resume sends at once");
    }

    #[tokio::test(start_paused = true)]
    async fn the_cap_counts_the_request_in_flight() {
        let r = rig(vec![Script::Wait { fail: false }], 100, 2);
        mark(&r.batch, "a");
        mark(&r.batch, "b");
        advance(TICK).await;
        assert_eq!(r.requests(), [ids(&["a", "b"])]);
        mark(&r.batch, "c");
        assert_eq!(r.batch.due(), 2, "a full batch takes no mark while its entries are in flight");
        assert_eq!(r.metrics.limiter_refresh_forgotten_cap_total(), 1);
        r.limiter.go.notify_one();
        settle().await;
        assert_eq!(r.batch.due(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_key_marked_again_while_in_flight_expires_one_lease_after_its_first_mark() {
        let r = rig(vec![Script::Wait { fail: true }], 100, 100);
        mark(&r.batch, "a");
        advance(TICK).await;
        advance(Duration::from_millis(500)).await;
        mark(&r.batch, "b");
        advance(Duration::from_millis(500)).await;
        mark(&r.batch, "a");
        r.batch.hold();
        r.limiter.go.notify_one();
        settle().await;
        assert_eq!(r.batch.due(), 2);
        // a was first marked at 0 s, b at 1.5 s; the lease is 20 s.
        advance(Duration::from_millis(18_500)).await;
        assert_eq!(r.batch.due(), 1, "a is given up at 20 s, behind the later mark of b");
        assert_eq!(r.metrics.limiter_refresh_forgotten_lease_expired_total(), 1);
    }

    #[test]
    fn the_config_states_the_pace_and_bounds() {
        let config = B2buaConfig::default();
        let lease = LimiterLease::starting_at(Duration::from_secs(120));
        let bounds = RefreshBatchConfig::from_config(&config, lease.clone());
        assert_eq!(bounds.tick, Duration::from_secs(1));
        assert_eq!(bounds.max, 1_000);
        assert_eq!(bounds.cap, 100_000);
        assert!(Arc::ptr_eq(&bounds.lease, &lease), "the worker's learnt lease");
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_batch_gives_up_its_entries_at_their_deadline_without_a_look() {
        let lease = LimiterLease::starting_at(Duration::from_secs(120));
        let r = rig_leased(Vec::new(), 100, 10, lease.clone());
        r.batch.hold();
        mark(&r.batch, "a");
        settle().await;
        advance(Duration::from_secs(10)).await;
        lease.learn(Duration::from_secs(30));
        settle().await;
        assert_eq!(r.metrics.limiter_refresh_due(), 1, "inside the new lease");
        advance(Duration::from_secs(21)).await;
        assert_eq!(
            r.metrics.limiter_refresh_forgotten_lease_expired_total(),
            1,
            "given up at 30 s by the sender, not at 120 s nor at the next look"
        );
        assert_eq!(r.metrics.limiter_refresh_due(), 0);
        mark(&r.batch, "b");
        settle().await;
        advance(Duration::from_secs(2)).await;
        lease.learn(Duration::from_secs(1));
        settle().await;
        assert_eq!(
            r.metrics.limiter_refresh_forgotten_lease_expired_total(),
            2,
            "a lease learnt shorter than an entry has waited gives it up at once"
        );
        assert!(r.requests().is_empty(), "a held batch sends nothing");
    }

    #[tokio::test(start_paused = true)]
    async fn a_lease_learnt_after_a_first_mark_moves_its_deadline() {
        let lease = LimiterLease::starting_at(Duration::from_secs(20));
        let r = rig_leased(Vec::new(), 100, 10, lease.clone());
        r.batch.hold();
        mark(&r.batch, "a");
        lease.learn(Duration::from_secs(60));
        advance(Duration::from_secs(30)).await;
        mark(&r.batch, "b");
        assert_eq!(r.batch.due(), 2, "a longer lease keeps the entry");
        lease.learn(Duration::from_secs(10));
        advance(Duration::from_secs(10)).await;
        assert_eq!(r.batch.due(), 0, "a shorter lease gives up both");
        assert_eq!(r.metrics.limiter_refresh_forgotten_lease_expired_total(), 2);
    }
}
