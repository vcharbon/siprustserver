use std::collections::VecDeque;

use async_trait::async_trait;
use sip_clock::testkit::settle;

use super::*;
use crate::limiter::testkit::{held_of, replies};
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
    /// One outcome per call, each stating the set sent (none when
    /// released, an empty one when dropped).
    Answer(Vec<RefreshOutcome>),
    /// One reply per call, as scripted.
    Replies(Vec<RefreshReply>),
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
    fn admit_budget(&self) -> std::time::Duration {
        crate::limiter::LOCAL_ADMIT_BUDGET
    }

    async fn admit(
        &self,
        _: &str,
        _: u64,
        _: &LimiterHeld,
        _: &[LimiterEntry],
        _: bool,
    ) -> AdmitOutcome {
        AdmitOutcome::NotSent
    }
    async fn release(&self, _: &[String]) -> ReleaseAnswer {
        ReleaseAnswer::Released
    }
    async fn refresh(&self, calls: &[RefreshCall]) -> RefreshAnswer {
        self.sent.lock().unwrap().push(calls.to_vec());
        let next = self.script.lock().unwrap().pop_front();
        match next {
            Some(Script::Answer(outcomes)) => RefreshAnswer::Answered(
                calls
                    .iter()
                    .zip(outcomes)
                    .map(|(call, outcome)| RefreshReply {
                        outcome,
                        held: match outcome {
                            RefreshOutcome::Released => None,
                            RefreshOutcome::Dropped => {
                                Some(LimiterHeld { change: call.held.change, entries: vec![] })
                            }
                            _ => Some(call.held.clone()),
                        },
                    })
                    .collect(),
            ),
            Some(Script::Replies(replies)) => RefreshAnswer::Answered(replies),
            Some(Script::Unavailable) => RefreshAnswer::Unavailable,
            Some(Script::Wait { fail }) => {
                self.go.notified().await;
                if fail {
                    return RefreshAnswer::Unavailable;
                }
                RefreshAnswer::Answered(replies(calls, RefreshOutcome::Extended))
            }
            Some(Script::Panic) => panic!("the limiter client panics"),
            None => RefreshAnswer::Answered(replies(calls, RefreshOutcome::Extended)),
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

/// Mark `key` due for call `c-<key>` holding `[x]`.
fn mark(batch: &RefreshBatch, key: &str) {
    batch.mark(key, &format!("c-{key}"), &held_of(&ids(&["x"])));
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
    assert_eq!(r.metrics.limiter().refresh_keys_sent_total(), 3);
    assert_eq!(r.metrics.limiter().refresh_answers_total(RefreshOutcome::Extended), 3);
    assert!(r.answers.lock().unwrap().is_empty(), "an extended answer is not handed back");
}

#[tokio::test(start_paused = true)]
async fn a_key_marked_again_while_due_is_sent_once_with_its_latest_held_set() {
    let r = rig(Vec::new(), 100, 100);
    r.batch.mark("a", "c-a", &held_of(&ids(&["x"])));
    r.batch.mark("a", "c-a", &held_of(&ids(&["x", "y"])));
    advance(TICK).await;
    let sent = r.limiter.sent.lock().unwrap().clone();
    assert_eq!(sent, [vec![RefreshCall { key: "a".into(), held: held_of(&ids(&["x", "y"])) }]]);
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
    for key in ["a", "b", "c", "d"] {
        mark(&r.batch, key);
    }
    advance(TICK).await;
    let answers = r.answers.lock().unwrap().clone();
    let x = held_of(&ids(&["x"]));
    let dropped = LimiterHeld { change: x.change, entries: vec![] };
    let expected = [
        ("b", RefreshOutcome::Reregistered, Some(x)),
        ("c", RefreshOutcome::Released, None),
        ("d", RefreshOutcome::Dropped, Some(dropped)),
    ]
    .map(|(key, outcome, held)| RefreshAnswered {
        call_ref: format!("c-{key}"),
        key: key.to_string(),
        outcome,
        held,
    });
    assert_eq!(answers, expected);
    assert_eq!(r.metrics.limiter().refresh_answers_total(RefreshOutcome::Reregistered), 1);
    assert_eq!(r.metrics.limiter().refresh_answers_total(RefreshOutcome::Released), 1);
    assert_eq!(r.metrics.limiter().refresh_answers_total(RefreshOutcome::Dropped), 1);
}

/// An `Extended` answer stating another set than the one the refresh
/// carried (an admit landed whose answer was lost) is handed back, so the
/// call learns what the limiter holds.
#[tokio::test(start_paused = true)]
async fn an_extended_answer_stating_another_set_is_handed_to_its_call() {
    let newer = LimiterHeld { change: 5, entries: held_of(&ids(&["y"])).entries };
    let replies = vec![
        RefreshReply { outcome: RefreshOutcome::Extended, held: Some(held_of(&ids(&["x"]))) },
        RefreshReply { outcome: RefreshOutcome::Extended, held: Some(newer.clone()) },
    ];
    let r = rig(vec![Script::Replies(replies)], 100, 100);
    mark(&r.batch, "a");
    mark(&r.batch, "b");
    advance(TICK).await;
    let answers = r.answers.lock().unwrap().clone();
    assert_eq!(
        answers,
        [RefreshAnswered {
            call_ref: "c-b".into(),
            key: "b".into(),
            outcome: RefreshOutcome::Extended,
            held: Some(newer),
        }],
        "only the answer stating another set"
    );
}

#[tokio::test(start_paused = true)]
async fn an_answer_travels_to_its_call_as_an_internal_event() {
    let answer = RefreshAnswered {
        call_ref: "c".into(),
        key: "c#k".into(),
        outcome: RefreshOutcome::Dropped,
        held: Some(LimiterHeld { change: 7, entries: vec![] }),
    };
    assert_eq!(RefreshAnswered::of(&answer.clone().into_event()), Some(answer));
    let other = CallEvent::Timer {
        timer_type: call::TimerType::LimiterRefresh,
        call_ref: "c".into(),
        leg_id: None,
        incarnation: None,
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
    assert_eq!(r.metrics.limiter().refresh_retries_total(), 2);
    advance(TICK).await;
    assert_eq!(r.requests()[1], ids(&["a", "b"]), "the oldest keys first, again");
    advance(TICK).await;
    assert_eq!(r.requests()[2..], [ids(&["a", "b"]), ids(&["c"])]);
    assert_eq!(r.batch.due(), 0);
    assert_eq!(
        r.metrics.limiter().refresh_keys_sent_total(),
        7,
        "two unanswered requests of two, then all three"
    );
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
    assert_eq!(r.metrics.limiter().refresh_given_up_total(RefreshGiveUp::Released), 2);
    r.batch.forget("never-marked");
    assert_eq!(r.metrics.limiter().refresh_given_up_total(RefreshGiveUp::Released), 2);
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
    assert_eq!(r.metrics.limiter().refresh_retries_total(), 1);
    advance(TICK).await;
    assert_eq!(r.requests()[1], ids(&["b"]));
}

#[tokio::test(start_paused = true)]
async fn a_key_marked_again_while_in_flight_keeps_its_new_mark_when_the_request_fails() {
    let r = rig(vec![Script::Wait { fail: true }], 100, 100);
    r.batch.mark("a", "c-a", &held_of(&ids(&["x"])));
    advance(TICK).await;
    r.batch.mark("a", "c-a", &held_of(&ids(&["x", "y"])));
    r.limiter.go.notify_one();
    settle().await;
    assert_eq!(r.batch.due(), 1);
    advance(TICK).await;
    let sent = r.limiter.sent.lock().unwrap().clone();
    assert_eq!(sent[1], [RefreshCall { key: "a".into(), held: held_of(&ids(&["x", "y"])) }]);
}

#[tokio::test(start_paused = true)]
async fn an_entry_due_for_one_lease_is_given_up_and_a_full_batch_gives_up_its_oldest() {
    let r = rig(Vec::new(), 100, 2);
    r.batch.hold();
    mark(&r.batch, "a");
    mark(&r.batch, "b");
    mark(&r.batch, "c");
    assert_eq!(r.batch.due(), 2);
    assert_eq!(r.metrics.limiter().refresh_given_up_total(RefreshGiveUp::Cap), 1);
    advance(Duration::from_secs(21)).await;
    assert_eq!(r.batch.due(), 0, "given up one lease after the first mark");
    assert_eq!(r.metrics.limiter().refresh_given_up_total(RefreshGiveUp::LeaseExpired), 2);
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
    assert_eq!(r.metrics.limiter().refresh_due(), 1);
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
    assert_eq!(r.metrics.limiter().task_restarts_total(LimiterTask::RefreshSender), 1);
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
    assert_eq!(r.metrics.limiter().refresh_answers_total(RefreshOutcome::Extended), 1);
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
    assert_eq!(r.metrics.limiter().refresh_given_up_total(RefreshGiveUp::Cap), 1);
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
    assert_eq!(r.metrics.limiter().refresh_given_up_total(RefreshGiveUp::LeaseExpired), 1);
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
    assert_eq!(r.metrics.limiter().refresh_due(), 1, "inside the new lease");
    advance(Duration::from_secs(21)).await;
    assert_eq!(
        r.metrics.limiter().refresh_given_up_total(RefreshGiveUp::LeaseExpired),
        1,
        "given up at 30 s by the sender, not at 120 s nor at the next look"
    );
    assert_eq!(r.metrics.limiter().refresh_due(), 0);
    mark(&r.batch, "b");
    settle().await;
    advance(Duration::from_secs(2)).await;
    lease.learn(Duration::from_secs(1));
    settle().await;
    assert_eq!(
        r.metrics.limiter().refresh_given_up_total(RefreshGiveUp::LeaseExpired),
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
    assert_eq!(r.metrics.limiter().refresh_given_up_total(RefreshGiveUp::LeaseExpired), 2);
}
