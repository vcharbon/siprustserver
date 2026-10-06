//! The tokio adapter around [`CallQueue`](super::CallQueue): the worker runs
//! what the queue hands it, isolates a failing body, lets a hung one be
//! aborted, holds a permit per body, and removes the queue on its release.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use super::*;

type Dispatcher = PerCallDispatcher<DispatchBody>;

/// A body that records `label` in `order`.
fn record(order: &Arc<Mutex<Vec<&'static str>>>, label: &'static str) -> DispatchBody {
    let order = order.clone();
    Box::pin(async move { order.lock().unwrap().push(label) })
}

/// Park a body on call `call_ref` until the returned gate opens.
async fn park(d: &Dispatcher, call_ref: &str) -> Arc<Notify> {
    park_as(d, call_ref, DispatchClass::OtherRequest).await
}

/// [`park`] with an event of `class`.
async fn park_as(d: &Dispatcher, call_ref: &str, class: DispatchClass) -> Arc<Notify> {
    let (gate, started) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let (g, st) = (gate.clone(), started.clone());
    let body: DispatchBody = Box::pin(async move {
        st.notify_one();
        g.notified().await;
    });
    let _ = d.offer(call_ref, body, class);
    started.notified().await;
    gate
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

async fn drained(d: &Dispatcher) {
    for _ in 0..1000 {
        if d.queue_count() == 0 {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the queue never drained");
}

/// The failures a failure hook heard, by call.
type Heard = Arc<Mutex<Vec<(String, HandlerFailure)>>>;

/// `d` with a failure hook recording what it hears.
fn failures(d: Dispatcher) -> (Dispatcher, Heard) {
    let heard = Arc::new(Mutex::new(Vec::new()));
    let h = heard.clone();
    let d = d.with_failure_hook(Arc::new(move |call_ref: &str, failure| {
        h.lock().unwrap().push((call_ref.to_string(), failure))
    }));
    (d, heard)
}

/// A panicking body is counted and reported; the worker survives it and
/// runs the call's next item.
#[tokio::test]
async fn a_panicking_body_is_reported_and_the_worker_runs_the_next() {
    let metrics = B2buaMetrics::new();
    let (d, heard) = failures(Dispatcher::new(1, 8, 1024, metrics.clone()));
    let order = Arc::new(Mutex::new(Vec::new()));
    let gate = park(&d, "c").await;
    let _ = d.offer("c", Box::pin(async { panic!("handler bug") }), DispatchClass::OtherRequest);
    let _ = d.offer("c", record(&order, "next"), DispatchClass::OtherRequest);
    gate.notify_one();
    d.release("c", RemovalClass::Terminated);
    drained(&d).await;
    assert_eq!(metrics.handler_panics_total(), 1);
    assert_eq!(*heard.lock().unwrap(), vec![("c".to_string(), HandlerFailure::Panicked)]);
    assert_eq!(*order.lock().unwrap(), vec!["next"]);
}

/// A hung body aborted from outside is dropped — releasing whatever it
/// holds — and reported; the worker runs the call's next item.
#[tokio::test]
async fn an_aborted_body_is_dropped_reported_and_the_worker_runs_the_next() {
    let (d, heard) = failures(Dispatcher::new(1, 8, 1024, B2buaMetrics::new()));
    let order = Arc::new(Mutex::new(Vec::new()));
    assert!(!d.abort_in_flight("c"), "no body in flight");
    let (dropped, started) = (Arc::new(AtomicBool::new(false)), Arc::new(Notify::new()));
    let (guard, st) = (DropFlag(dropped.clone()), started.clone());
    let hung: DispatchBody = Box::pin(async move {
        let _guard = guard;
        st.notify_one();
        std::future::pending::<()>().await;
    });
    let _ = d.offer("c", hung, DispatchClass::OtherRequest);
    let _ = d.offer("c", record(&order, "next"), DispatchClass::OtherRequest);
    started.notified().await;
    assert!(d.in_flight().abort("c"));
    d.release("c", RemovalClass::Terminated);
    drained(&d).await;
    assert!(dropped.load(Ordering::SeqCst), "the hung body is dropped");
    assert_eq!(*heard.lock().unwrap(), vec![("c".to_string(), HandlerFailure::Aborted)]);
    assert_eq!(*order.lock().unwrap(), vec!["next"]);
}

/// Sets its flag when dropped.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// An item whose body flags its first poll, and which wakes `made` when the
/// worker makes its body.
struct Probe {
    made: Arc<Notify>,
    polled: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

impl Runnable for Probe {
    fn into_body(self) -> DispatchBody {
        self.made.notify_one();
        let (polled, guard) = (self.polled, DropFlag(self.dropped));
        Box::pin(async move {
            let _guard = guard;
            polled.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
        })
    }
}

/// A body aborted between its spawn and its first poll never runs: it is
/// dropped unpolled, and reported aborted like any other.
#[tokio::test(flavor = "current_thread")]
async fn a_body_aborted_before_its_first_poll_is_dropped_unpolled() {
    let d = PerCallDispatcher::<Probe>::new(1, 8, 1024, B2buaMetrics::new());
    let heard = Arc::new(Mutex::new(Vec::new()));
    let h = heard.clone();
    let d = d.with_failure_hook(Arc::new(move |_: &str, failure| h.lock().unwrap().push(failure)));
    let (made, polled, dropped) = (
        Arc::new(Notify::new()),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let probe = Probe { made: made.clone(), polled: polled.clone(), dropped: dropped.clone() };
    // Woken as the worker makes the body, this task is scheduled ahead of
    // the body's task, spawned just after: the abort lands before the first
    // poll.
    let aborter = {
        let (d, made) = (d.clone(), made.clone());
        tokio::spawn(async move {
            made.notified().await;
            d.abort_in_flight("c")
        })
    };
    tokio::task::yield_now().await;
    let _ = d.offer("c", probe, DispatchClass::OtherRequest);
    assert!(aborter.await.unwrap(), "the body is in flight");
    d.release("c", RemovalClass::Terminated);
    for _ in 0..100 {
        if d.queue_count() == 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(d.queue_count(), 0);
    assert!(!polled.load(Ordering::SeqCst), "the body never ran");
    assert!(dropped.load(Ordering::SeqCst), "the body is dropped");
    assert_eq!(*heard.lock().unwrap(), vec![HandlerFailure::Aborted]);
}

/// Every permit held, a call's item waits for one — counted as saturation —
/// and runs once a body anywhere finishes. A body holds its permit for its
/// whole run.
#[tokio::test]
async fn a_body_waits_for_a_permit_while_every_permit_is_held() {
    let metrics = B2buaMetrics::new();
    let d = Dispatcher::new(1, 8, 1024, metrics.clone());
    let order = Arc::new(Mutex::new(Vec::new()));
    let gate = park(&d, "a").await;
    let _ = d.offer("b", record(&order, "b"), DispatchClass::OtherRequest);
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(order.lock().unwrap().is_empty(), "b waits for the permit a holds");
    assert_eq!(metrics.saturation_total(), 1);
    gate.notify_one();
    d.release("a", RemovalClass::Terminated);
    d.release("b", RemovalClass::Terminated);
    drained(&d).await;
    assert_eq!(*order.lock().unwrap(), vec!["b"]);
}

/// A worker runs its call's items in their order and exits on the release,
/// a full queue's included, taking the queue away. The removal is counted
/// once, under the first release's class.
#[tokio::test]
async fn a_release_removes_the_queue_once_its_items_ran() {
    let metrics = B2buaMetrics::new();
    let d = Dispatcher::new(1, 1, 1024, metrics.clone());
    let order = Arc::new(Mutex::new(Vec::new()));
    let gate = park(&d, "a").await;
    let _ = d.offer("a", record(&order, "queued"), DispatchClass::OtherRequest);
    let _ = d.offer("a", record(&order, "timer"), DispatchClass::Timer);
    d.release("a", RemovalClass::Terminated);
    d.release("a", RemovalClass::Orphan);
    let _ = d.offer("b", Box::pin(async {}), DispatchClass::OtherRequest);
    d.release("b", RemovalClass::Orphan);
    gate.notify_one();
    drained(&d).await;
    assert_eq!(*order.lock().unwrap(), vec!["queued", "timer"]);
    assert_eq!(metrics.creations_total(), 2);
    assert_eq!(metrics.removals_total(), 2);
    assert_eq!(metrics.removals_of_total(RemovalClass::Terminated), 1);
    assert_eq!(metrics.removals_of_total(RemovalClass::Orphan), 1);
    let text = metrics.prometheus_text();
    assert!(text.contains("b2bua_call_removals_by_class_total{class=\"terminated\"} 1"));
    assert!(text.contains("b2bua_call_removals_by_class_total{class=\"orphan\"} 1"));
}

/// At the global cap an event of a bounded class finds no queue to open: it
/// is handed back owing its row's answer and counted. An event that may
/// wait past bounds opens its call's queue past the cap, counted too.
#[tokio::test]
async fn at_the_global_cap_only_events_past_bounds_open_a_queue() {
    let metrics = B2buaMetrics::new();
    let d = Dispatcher::new(1, 8, 1, metrics.clone());
    let order = Arc::new(Mutex::new(Vec::new()));
    let gate = park(&d, "c").await;
    let refused = d.offer("n", record(&order, "invite"), DispatchClass::InitialInvite);
    match refused.outcome {
        Outcome::Discarded(Discarded { why, owed, .. }) => {
            assert_eq!((why, owed), (Discard::AtCap, Owed::NewCallRefused));
        }
        Outcome::Queued => panic!("a bounded event opened a queue at the cap"),
    }
    assert_eq!(metrics.cap_drops_total(), 0, "a new call is counted by the router that answers it");
    let other = d.offer("o", record(&order, "info"), DispatchClass::OtherRequest);
    assert!(matches!(other.outcome, Outcome::Discarded(Discarded { why: Discard::AtCap, .. })));
    assert_eq!(metrics.cap_drops_total(), 1);

    let past = d.offer("n", record(&order, "cancelled"), DispatchClass::Cancelled);
    assert!(matches!(past.outcome, Outcome::Queued));
    assert_eq!(metrics.past_bound_of_total(PastBound::Cap), 1);
    d.release("n", RemovalClass::Orphan);
    gate.notify_one();
    d.release("c", RemovalClass::Terminated);
    drained(&d).await;
    assert_eq!(*order.lock().unwrap(), vec!["cancelled"]);
}

/// New normal calls hold at most their share of the permits: past it the
/// next new call's body waits, holding no shared permit, while an emergency
/// call's and an established call's bodies take the shared permits left.
#[tokio::test]
async fn new_calls_hold_their_share_of_the_permits_and_no_more() {
    let metrics = B2buaMetrics::new();
    let d = Dispatcher::new(4, 8, 1024, metrics.clone()).with_new_call_bounds(2, 0);
    let order = Arc::new(Mutex::new(Vec::new()));
    let gates = [
        park_as(&d, "n1", DispatchClass::InitialInvite).await,
        park_as(&d, "n2", DispatchClass::InitialInvite).await,
    ];
    let _ = d.offer("n3", record(&order, "n3"), DispatchClass::InitialInvite);
    settle().await;
    assert!(order.lock().unwrap().is_empty(), "a third new call waits for the share");
    assert_eq!((metrics.new_call_share_waits_total(), metrics.saturation_total()), (1, 0));
    let emergency = park_as(&d, "e", DispatchClass::EmergencyInvite).await;
    let _ = d.offer("c", record(&order, "bye"), DispatchClass::OtherRequest);
    settle().await;
    assert_eq!(*order.lock().unwrap(), vec!["bye"], "the last shared permit is free");
    assert_eq!(metrics.saturation_total(), 0, "no body waited on the shared pool");

    gates[0].notify_one();
    settle().await;
    assert_eq!(*order.lock().unwrap(), vec!["bye", "n3"], "a freed share runs the next");
    gates[1].notify_one();
    emergency.notify_one();
    for call in ["n1", "n2", "n3", "e", "c"] {
        d.release(call, RemovalClass::Terminated);
    }
    drained(&d).await;
}

/// A new call's body holds a shared permit besides its new-call one: with
/// every shared permit held, it waits though its share is free, so the
/// bodies in flight never pass the shared pool.
#[tokio::test]
async fn a_new_call_also_holds_a_shared_permit() {
    let metrics = B2buaMetrics::new();
    let d = Dispatcher::new(2, 8, 1024, metrics.clone()).with_new_call_bounds(2, 0);
    let order = Arc::new(Mutex::new(Vec::new()));
    let gates = [park(&d, "a").await, park(&d, "b").await];
    let _ = d.offer("n", record(&order, "invite"), DispatchClass::InitialInvite);
    settle().await;
    assert!(order.lock().unwrap().is_empty(), "the new call waits for a shared permit");
    assert_eq!((metrics.new_call_share_waits_total(), metrics.saturation_total()), (0, 1));

    gates[0].notify_one();
    settle().await;
    assert_eq!(*order.lock().unwrap(), vec!["invite"], "a freed shared permit runs it");
    gates[1].notify_one();
    for call in ["a", "b", "n"] {
        d.release(call, RemovalClass::Terminated);
    }
    drained(&d).await;
}

/// A normal new INVITE opens a queue only below the cap less the new-call
/// headroom; an emergency INVITE and an in-dialog request open one up to the
/// cap, and past it only events past bounds do.
#[tokio::test]
async fn a_new_normal_call_leaves_the_headroom_to_every_other_event() {
    let metrics = B2buaMetrics::new();
    let d = Dispatcher::new(8, 8, 4, metrics.clone()).with_new_call_bounds(8, 2);
    let gates = [park(&d, "a").await, park(&d, "b").await];
    let refused = d.offer("n", Box::pin(async {}), DispatchClass::InitialInvite);
    match refused.outcome {
        Outcome::Discarded(Discarded { why, owed, .. }) => {
            assert_eq!((why, owed), (Discard::AtCap, Owed::NewCallRefused));
        }
        Outcome::Queued => panic!("a normal new INVITE opened a queue in the headroom"),
    }
    let emergency = d.offer("e", Box::pin(async {}), DispatchClass::EmergencyInvite);
    assert!(matches!(emergency.outcome, Outcome::Queued), "emergency passes the headroom");
    let taken_over = d.offer("t", Box::pin(async {}), DispatchClass::OtherRequest);
    assert!(matches!(taken_over.outcome, Outcome::Queued), "in-dialog passes the headroom");
    let at_cap = d.offer("x", Box::pin(async {}), DispatchClass::EmergencyInvite);
    assert!(matches!(at_cap.outcome, Outcome::Discarded(Discarded { why: Discard::AtCap, .. })));
    assert_eq!(metrics.cap_drops_total(), 0, "a new call is counted by the router that answers it");

    for gate in gates {
        gate.notify_one();
    }
    // Each call ends after its new INVITE ran: a release ahead of it would
    // leave it to the call's next queue.
    settle().await;
    for call in ["a", "b", "e", "t"] {
        d.release(call, RemovalClass::Terminated);
    }
    drained(&d).await;
}

/// A call is counted near its lifetime cap once its counted offers pass
/// 80 % of it, and leaves the count with its queue. The offer that crosses
/// the cap says so, once, and is counted.
#[tokio::test]
async fn a_call_near_its_lifetime_cap_is_counted_until_released() {
    let metrics = B2buaMetrics::new();
    let d = Dispatcher::new(1, 64, 1024, metrics.clone()).with_lifetime_cap(10);
    let gate = park(&d, "c").await;
    for _ in 0..7 {
        let _ = d.offer("c", Box::pin(async {}), DispatchClass::OtherRequest);
    }
    assert_eq!(metrics.calls_near_lifetime_cap(), 0, "8 offers of 10 is not past 80 %");
    let _ = d.offer("c", Box::pin(async {}), DispatchClass::OtherRequest);
    let _ = d.offer("c", Box::pin(async {}), DispatchClass::OtherRequest);
    assert_eq!(metrics.calls_near_lifetime_cap(), 1, "counted once");
    let crossing = d.offer("c", Box::pin(async {}), DispatchClass::OtherRequest);
    assert!(crossing.crossed_lifetime_cap);
    assert!(!d.offer("c", Box::pin(async {}), DispatchClass::Response).crossed_lifetime_cap);
    assert_eq!(metrics.message_cap_lifetime_crossed_total(), 1);

    gate.notify_one();
    d.release("c", RemovalClass::Terminated);
    drained(&d).await;
    assert_eq!(metrics.calls_near_lifetime_cap(), 0, "the released call leaves the count");
}

/// A body that records `label` in `order` with the queue removals counted
/// when it runs: which side of a release it ran on.
fn record_side(
    order: &Arc<Mutex<Vec<(&'static str, u64)>>>,
    metrics: &B2buaMetrics,
    label: &'static str,
) -> DispatchBody {
    let (order, metrics) = (order.clone(), metrics.clone());
    Box::pin(async move { order.lock().unwrap().push((label, metrics.removals_total())) })
}

/// A new call never runs on the queue its identity's previous call releases:
/// one queued when the release comes waits behind it with every item queued
/// after it, and one offered behind the release joins them. The worker then
/// runs them on the call's next queue: one removal, one creation more.
#[tokio::test]
async fn a_new_call_waits_for_its_identitys_release_and_runs_on_the_next_queue() {
    let metrics = B2buaMetrics::new();
    let d = Dispatcher::new(1, 8, 1024, metrics.clone());
    let order = Arc::new(Mutex::new(Vec::new()));
    let side = |label| record_side(&order, &metrics, label);
    let gate = park(&d, "c").await;
    let _ = d.offer("c", side("ending"), DispatchClass::OtherRequest);
    let _ = d.offer("c", side("retry"), DispatchClass::InitialInvite);
    let _ = d.offer("c", side("retry's cancel"), DispatchClass::Cancelled);
    d.release("c", RemovalClass::Terminated);
    let behind = d.offer("c", side("next"), DispatchClass::InitialInvite);
    assert!(matches!(behind.outcome, Outcome::Queued), "a new call behind a release waits");
    let after = d.offer("c", side("next's response"), DispatchClass::Response);
    assert!(matches!(after.outcome, Outcome::Queued), "so does what follows it");

    gate.notify_one();
    settle().await;
    assert_eq!(
        *order.lock().unwrap(),
        vec![
            ("ending", 0),
            ("retry", 1),
            ("retry's cancel", 1),
            ("next", 1),
            ("next's response", 1)
        ],
        "the release is taken before the first new call runs"
    );
    assert!(d.has_queue("c"), "the next queue lives on");
    assert_eq!(
        metrics.removals_of_total(RemovalClass::Terminated),
        1,
        "the first queue is removed"
    );
    assert_eq!(metrics.creations_total(), 2, "the next queue is created");

    d.release("c", RemovalClass::Orphan);
    drained(&d).await;
    assert_eq!(metrics.removals_total(), 2);
}

/// Behind a release with no new call waiting, an offer is discarded as
/// before: the call's own stragglers never open its next queue.
#[tokio::test]
async fn behind_a_release_with_no_new_call_waiting_an_event_is_discarded() {
    let metrics = B2buaMetrics::new();
    let d = Dispatcher::new(1, 8, 1024, metrics.clone());
    let gate = park(&d, "c").await;
    d.release("c", RemovalClass::Terminated);
    let late = d.offer("c", Box::pin(async {}), DispatchClass::OtherRequest);
    assert!(matches!(
        late.outcome,
        Outcome::Discarded(Discarded { why: Discard::Released(RemovalClass::Terminated), .. })
    ));
    gate.notify_one();
    drained(&d).await;
    assert_eq!(metrics.creations_total(), 1);
    assert_eq!(metrics.removals_total(), 1);
}
