//! The slow axis of the worker set (ADR-0069): an endpoint flip while a late
//! answer is fresh reads the worker `Slow` and keeps it routed, a `Slow` worker
//! turning `Dead` is a verdict, and one turning `Alive` is a recovery.

use std::time::Duration;

use super::*;
use topology::SimulatedMembership;

const FRESH: Duration = Duration::from_millis(2_000);

fn ws(initial: &[(&str, &str)]) -> (WorkerSet, SimulatedMembership) {
    let sim = SimulatedMembership::with_clock(
        initial.iter().map(|(o, h)| Peer::new(*o, *h)).collect(),
        Clock::test_at(0),
    );
    let set = WorkerSet::new(Arc::new(sim.clone()), 5060, Clock::test_at(0));
    (set, sim)
}

fn told(rx: &mut broadcast::Receiver<WorkerId>) -> Vec<WorkerId> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

fn health(set: &WorkerSet, id: &str) -> Option<WorkerHealth> {
    set.resolve(id).map(|w| w.health)
}

/// A worker read `Alive` whose endpoint flips not ready, not terminating,
/// while its late answer is fresh, reads `Slow` and stays routed, with no
/// verdict and no tombstone; it is no member `read_dead_now` names. Read `Dead`
/// by its probe later, it is one verdict and leaves the routing.
#[tokio::test(start_paused = true)]
async fn a_flip_while_a_late_answer_is_fresh_reads_slow_until_the_probe_reads_it_dead() {
    let (set, sim) = ws(&[("w0", "192.0.2.1"), ("w1", "192.0.2.2")]);
    set.set_health("w0", WorkerHealth::Alive);
    set.set_health("w1", WorkerHealth::Alive);
    let mut verdicts = set.read_dead();
    set.note_late_answer("w1", FRESH);
    tokio::time::advance(FRESH).await;
    sim.set_conditions("w1", false, false);
    set.recompose();
    assert_eq!(health(&set, "w1"), Some(WorkerHealth::Slow));
    assert_eq!(told(&mut verdicts), Vec::<WorkerId>::new());
    assert!(set.read_dead_now().is_empty());
    assert!(set.away_ordinals().is_empty(), "a held worker is routed");

    set.set_health("w1", WorkerHealth::Alive);
    assert_eq!(health(&set, "w1"), Some(WorkerHealth::Slow), "held while its endpoint reads down");
    tokio::time::advance(Duration::from_secs(60)).await;
    set.recompose();
    assert_eq!(health(&set, "w1"), Some(WorkerHealth::Slow), "held while it reads slow");

    set.set_health("w1", WorkerHealth::Dead);
    assert!(set.resolve("w1").is_none(), "out of the routing once dead");
    assert_eq!(told(&mut verdicts), ["w1"]);
    let addr = ProxyAddr::new("192.0.2.2", 5060);
    assert_eq!(set.lookup_by_address(&addr).map(|w| w.health), Some(WorkerHealth::Dead));
}

/// A flip once the late answer's freshness ran out is the dead verdict it is
/// with no late answer.
#[tokio::test(start_paused = true)]
async fn a_flip_after_the_late_answer_went_stale_is_a_dead_verdict() {
    let (set, sim) = ws(&[("w0", "192.0.2.1"), ("w1", "192.0.2.2")]);
    set.set_health("w0", WorkerHealth::Alive);
    set.set_health("w1", WorkerHealth::Alive);
    let mut verdicts = set.read_dead();
    set.note_late_answer("w1", FRESH);
    tokio::time::advance(FRESH + Duration::from_millis(1)).await;
    sim.set_conditions("w1", false, false);
    set.recompose();
    assert!(set.resolve("w1").is_none());
    assert_eq!(told(&mut verdicts), ["w1"]);
}

/// A held worker whose endpoint reads ready again is no longer held: the
/// probe's next answers read it `Alive`.
#[tokio::test(start_paused = true)]
async fn a_held_worker_ready_again_reads_alive_on_its_answers() {
    let (set, sim) = ws(&[("w0", "192.0.2.1"), ("w1", "192.0.2.2")]);
    set.set_health("w0", WorkerHealth::Alive);
    set.set_health("w1", WorkerHealth::Alive);
    set.note_late_answer("w1", FRESH);
    sim.set_conditions("w1", false, false);
    set.recompose();
    assert_eq!(health(&set, "w1"), Some(WorkerHealth::Slow));
    sim.set_conditions("w1", true, false);
    set.recompose();
    set.set_health("w1", WorkerHealth::Alive);
    assert_eq!(health(&set, "w1"), Some(WorkerHealth::Alive));
}

/// `Slow` to `Dead` is a dead verdict, `Slow` to `Alive` a recovery; `Alive`
/// to `Slow` is neither.
#[test]
fn slow_to_dead_is_a_verdict_and_slow_to_alive_a_recovery() {
    let (set, _sim) = ws(&[("w0", "192.0.2.1"), ("w1", "192.0.2.2")]);
    set.set_health("w0", WorkerHealth::Alive);
    set.set_health("w1", WorkerHealth::Alive);
    let mut verdicts = set.read_dead();
    let mut recoveries = set.read_recovered();
    set.set_health("w1", WorkerHealth::Slow);
    assert_eq!(told(&mut verdicts), Vec::<WorkerId>::new());
    assert_eq!(told(&mut recoveries), Vec::<WorkerId>::new());
    set.set_health("w1", WorkerHealth::Alive);
    assert_eq!(told(&mut recoveries), ["w1"]);
    set.set_health("w1", WorkerHealth::Slow);
    set.set_health("w1", WorkerHealth::Dead);
    assert_eq!(told(&mut verdicts), ["w1"]);
    set.set_health("w1", WorkerHealth::Alive);
    assert_eq!(told(&mut recoveries), Vec::<WorkerId>::new(), "dead to alive is no recovery");
}

/// A view where the only worker answering reads `Slow` is no blind one: a
/// worker read `Alive` from it keeps the others' dead verdicts.
#[test]
fn a_slow_worker_keeps_the_view_from_reading_blind() {
    let (set, _sim) = ws(&[("w0", "192.0.2.1"), ("w1", "192.0.2.2"), ("w2", "192.0.2.3")]);
    set.set_health("w0", WorkerHealth::Alive);
    set.set_health("w1", WorkerHealth::Slow);
    set.set_health("w2", WorkerHealth::Dead);
    set.set_health("w0", WorkerHealth::Dead);
    set.set_health("w0", WorkerHealth::Alive);
    assert_eq!(health(&set, "w2"), Some(WorkerHealth::Dead));
}

/// A view where no member reads ready routes on the probe. A worker read
/// `Alive` turning `Dead` there is a verdict (`w2`, the control); one read
/// `Alive`, then `Slow`, then `Dead` is the same death and tells the same.
#[test]
fn slow_to_dead_in_a_view_with_no_ready_member_is_a_verdict() {
    let (set, sim) = ws(&[("w0", "192.0.2.1"), ("w1", "192.0.2.2"), ("w2", "192.0.2.3")]);
    for id in ["w0", "w1", "w2"] {
        set.set_health(id, WorkerHealth::Alive);
    }
    for id in ["w0", "w1", "w2"] {
        sim.set_conditions(id, false, false);
    }
    set.recompose();
    assert_eq!(health(&set, "w1"), Some(WorkerHealth::Alive), "routed on its answers");
    let mut verdicts = set.read_dead();
    set.set_health("w2", WorkerHealth::Dead);
    assert_eq!(told(&mut verdicts), ["w2"], "alive to dead with no member ready");
    set.set_health("w1", WorkerHealth::Slow);
    set.set_health("w1", WorkerHealth::Dead);
    assert_eq!(told(&mut verdicts), ["w1"], "alive, slow, then dead with no member ready");
}

/// In a view where no member reads ready, the proxy reached no worker and read
/// each `Dead`; at the heal a worker read `Alive` returns the others to
/// `Unknown`. One of them answering late reads `Slow`, routed with no verdict,
/// so its dialogs stay on it and no backup serves them beside it; its silence
/// after is its dead verdict.
#[test]
fn a_worker_answering_late_at_a_heal_with_no_ready_member_reads_slow() {
    let (set, sim) = ws(&[("w0", "192.0.2.1"), ("w1", "192.0.2.2"), ("w2", "192.0.2.3")]);
    for id in ["w0", "w1", "w2"] {
        set.set_health(id, WorkerHealth::Alive);
        sim.set_conditions(id, false, false);
    }
    set.recompose();
    for id in ["w0", "w1", "w2"] {
        set.set_health(id, WorkerHealth::Dead);
    }
    let mut verdicts = set.read_dead();
    set.set_health("w0", WorkerHealth::Alive);
    assert_eq!(
        health(&set, "w1"),
        Some(WorkerHealth::Unknown),
        "a verdict taken blind is forgotten"
    );
    set.set_health("w1", WorkerHealth::Slow);
    assert_eq!(health(&set, "w1"), Some(WorkerHealth::Slow), "routed on its late answers");
    assert_eq!(told(&mut verdicts), Vec::<WorkerId>::new());
    set.set_health("w1", WorkerHealth::Dead);
    assert_eq!(told(&mut verdicts), ["w1"], "slow, then dead with no member ready");
}
