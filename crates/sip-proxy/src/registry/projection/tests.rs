use std::time::Duration;

use super::*;
use topology::SimulatedMembership;

fn ws(initial: &[(&str, &str)], port: u16) -> (WorkerSet, SimulatedMembership) {
    let sim = SimulatedMembership::with_clock(
        initial.iter().map(|(o, h)| Peer::new(*o, *h)).collect(),
        Clock::test_at(0),
    );
    let set = WorkerSet::new(Arc::new(sim.clone()), port, Clock::test_at(0));
    (set, sim)
}

#[test]
fn new_workers_start_unknown_with_default_port() {
    let (set, _sim) = ws(&[("w0", "10.0.0.1")], 5060);
    let w0 = set.resolve("w0").unwrap();
    assert_eq!(w0.health, WorkerHealth::Unknown);
    assert_eq!(w0.address, ProxyAddr::new("10.0.0.1", 5060));
}

#[test]
fn health_write_composes_and_survives_membership_recompose() {
    let (set, sim) = ws(&[("w0", "10.0.0.1")], 5060);
    set.set_health("w0", WorkerHealth::Alive);
    assert_eq!(set.resolve("w0").unwrap().health, WorkerHealth::Alive);
    // A membership change (new peer) must not stomp w0's probe-written health.
    sim.add(Peer::new("w1", "10.0.0.2"));
    set.recompose();
    assert_eq!(set.resolve("w0").unwrap().health, WorkerHealth::Alive);
    assert_eq!(set.resolve("w1").unwrap().health, WorkerHealth::Unknown);
}

#[test]
fn a_worker_read_alive_out_of_a_view_reading_none_alive_forgets_the_others_dead_verdicts() {
    let (set, _sim) = ws(&[("w0", "10.0.0.1"), ("w1", "10.0.0.2"), ("w2", "10.0.0.3")], 5060);
    set.set_health("w0", WorkerHealth::Dead);
    set.set_health("w1", WorkerHealth::Dead);
    set.set_health("w2", WorkerHealth::Dead);
    set.set_health("w1", WorkerHealth::Alive);
    assert_eq!(set.resolve("w0").unwrap().health, WorkerHealth::Unknown);
    assert_eq!(set.resolve("w1").unwrap().health, WorkerHealth::Alive);
    assert_eq!(set.resolve("w2").unwrap().health, WorkerHealth::Unknown);
    set.set_health("w0", WorkerHealth::Dead);
    assert_eq!(set.resolve("w0").unwrap().health, WorkerHealth::Dead, "the probe judges afresh");
}

#[test]
fn a_dead_verdict_taken_while_a_worker_read_alive_stands() {
    let (set, _sim) = ws(&[("w0", "10.0.0.1"), ("w1", "10.0.0.2"), ("w2", "10.0.0.3")], 5060);
    set.set_health("w0", WorkerHealth::Alive);
    set.set_health("w1", WorkerHealth::Dead);
    set.set_health("w2", WorkerHealth::Alive);
    assert_eq!(set.resolve("w1").unwrap().health, WorkerHealth::Dead);
}

#[test]
fn address_move_resets_health() {
    let (set, sim) = ws(&[("w0", "10.0.0.1")], 5060);
    set.set_health("w0", WorkerHealth::Dead);
    sim.change_address(Peer::new("w0", "10.0.0.2"));
    set.recompose();
    let w0 = set.resolve("w0").unwrap();
    assert_eq!(w0.address, ProxyAddr::new("10.0.0.2", 5060));
    assert_eq!(w0.health, WorkerHealth::Unknown, "a stale Dead must not survive a restart");
}

#[test]
fn departed_worker_pruned_on_recompose() {
    let (set, sim) = ws(&[("w0", "10.0.0.1"), ("w1", "10.0.0.2")], 5060);
    sim.remove("w1");
    set.recompose();
    assert!(set.resolve("w1").is_none());
    assert!(set.resolve("w0").is_some());
}

#[tokio::test(start_paused = true)]
async fn a_departed_address_reads_dead_for_timer_h_then_nothing() {
    let (set, sim) = ws(&[("w0", "10.0.0.1"), ("w1", "10.0.0.2")], 5060);
    set.set_health("w1", WorkerHealth::Alive);
    let gone = ProxyAddr::new("10.0.0.2", 5060);
    sim.remove("w1");
    set.recompose();

    let dead = set.lookup_by_address(&gone).expect("the departed address still resolves");
    assert_eq!(dead.id, "w1");
    assert_eq!(dead.health, WorkerHealth::Dead);
    assert!(set.resolve("w1").is_none(), "the ordinal stays gone on the request path");

    tokio::time::advance(Duration::from_millis(DEPARTED_ADDRESS_TTL_MS - 1)).await;
    assert!(set.lookup_by_address(&gone).is_some(), "inside the window");
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(set.lookup_by_address(&gone).is_none(), "the window closed at Timer H");
}

#[test]
fn a_host_move_tombstones_the_old_address_and_leaves_the_new_one_unknown() {
    let (set, sim) = ws(&[("w0", "10.0.0.1")], 5060);
    set.set_health("w0", WorkerHealth::Alive);
    sim.change_address(Peer::new("w0", "10.0.0.2"));
    set.recompose();

    let old = set
        .lookup_by_address(&ProxyAddr::new("10.0.0.1", 5060))
        .expect("the host it moved off is tombstoned");
    assert_eq!((old.id.as_str(), old.health), ("w0", WorkerHealth::Dead));
    let new = set.lookup_by_address(&ProxyAddr::new("10.0.0.2", 5060)).expect("the new host");
    assert_eq!(new.health, WorkerHealth::Unknown, "the fresh endpoint awaits its probe");
    assert_eq!(set.resolve("w0").unwrap().address, ProxyAddr::new("10.0.0.2", 5060));

    // The tombstone is address-keyed, so the live ordinal's health writes
    // never reach it.
    set.set_health("w0", WorkerHealth::Alive);
    assert_eq!(
        set.lookup_by_address(&ProxyAddr::new("10.0.0.1", 5060)).unwrap().health,
        WorkerHealth::Dead
    );
}

#[test]
fn a_join_at_a_tombstoned_address_clears_it() {
    let (set, sim) = ws(&[("w0", "10.0.0.1")], 5060);
    let addr = ProxyAddr::new("10.0.0.1", 5060);
    sim.remove("w0");
    set.recompose();
    assert_eq!(set.lookup_by_address(&addr).unwrap().health, WorkerHealth::Dead);

    sim.add(Peer::new("w9", "10.0.0.1"));
    set.recompose();
    let joined = set.lookup_by_address(&addr).expect("the new tenant of the address");
    assert_eq!((joined.id.as_str(), joined.health), ("w9", WorkerHealth::Unknown));
}

#[test]
fn a_member_no_longer_ready_departs_and_its_address_reads_dead() {
    let (set, sim) = ws(&[("w0", "10.0.0.1"), ("w1", "10.0.0.2")], 5060);
    set.set_health("w1", WorkerHealth::Alive);
    sim.set_conditions("w1", false, true);
    set.recompose();

    assert!(set.resolve("w1").is_none(), "a not-ready member is out of the projection");
    assert_eq!(set.snapshot().len(), 1);
    let gone = set
        .lookup_by_address(&ProxyAddr::new("10.0.0.2", 5060))
        .expect("the not-ready member's address is tombstoned");
    assert_eq!((gone.id.as_str(), gone.health), ("w1", WorkerHealth::Dead));
    assert!(sim.resolve("w1").is_some(), "membership still holds it — it is only unroutable");
}

#[test]
fn a_member_ready_again_joins_fresh_and_clears_its_tombstone() {
    let (set, sim) = ws(&[("w0", "10.0.0.1")], 5060);
    set.set_health("w0", WorkerHealth::Alive);
    sim.set_conditions("w0", false, false);
    set.recompose();
    let addr = ProxyAddr::new("10.0.0.1", 5060);
    assert_eq!(set.lookup_by_address(&addr).unwrap().health, WorkerHealth::Dead);

    sim.set_conditions("w0", true, false);
    set.recompose();
    let back = set.resolve("w0").expect("routable again");
    assert_eq!(back.health, WorkerHealth::Unknown, "a returning endpoint awaits its probe");
    let live = set.lookup_by_address(&addr).unwrap();
    assert_eq!((live.id.as_str(), live.health), ("w0", WorkerHealth::Unknown));
}

#[test]
fn a_terminating_flip_on_a_ready_member_changes_nothing() {
    let (set, sim) = ws(&[("w0", "10.0.0.1")], 5060);
    set.set_health("w0", WorkerHealth::Alive);
    let before = set.resolve("w0").unwrap();
    sim.set_conditions("w0", true, true);
    set.recompose();
    assert_eq!(set.resolve("w0").unwrap(), before, "health and acceptance kept");
    assert_eq!(
        set.lookup_by_address(&ProxyAddr::new("10.0.0.1", 5060)).unwrap().health,
        WorkerHealth::Alive,
        "no tombstone for an address still served"
    );
}

/// The verdicts a reader has been told so far.
fn told(rx: &mut broadcast::Receiver<WorkerId>) -> Vec<WorkerId> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

#[test]
fn a_worker_read_alive_flipping_not_ready_is_one_dead_verdict() {
    let (set, sim) = ws(&[("w0", "10.0.0.1"), ("w1", "10.0.0.2")], 5060);
    set.set_health("w1", WorkerHealth::Alive);
    let mut verdicts = set.read_dead();
    sim.set_conditions("w1", false, false);
    set.recompose();
    assert!(set.resolve("w1").is_none(), "told once the projection holds the flip");
    assert_eq!(told(&mut verdicts), ["w1"]);

    set.recompose();
    sim.set_conditions("w1", false, true);
    set.recompose();
    assert_eq!(told(&mut verdicts), Vec::<WorkerId>::new(), "one verdict per flip");
}

#[test]
fn a_flip_of_a_worker_not_read_alive_is_no_verdict() {
    let (set, sim) = ws(&[("w0", "10.0.0.1"), ("w1", "10.0.0.2"), ("w2", "10.0.0.3")], 5060);
    set.set_health("w0", WorkerHealth::Alive);
    set.set_health("w2", WorkerHealth::Dead);
    let mut verdicts = set.read_dead();
    sim.set_conditions("w1", false, false);
    sim.set_conditions("w2", false, false);
    set.recompose();
    assert_eq!(told(&mut verdicts), Vec::<WorkerId>::new(), "unknown, and already read dead");
}

#[test]
fn a_terminating_flip_and_a_departure_are_no_verdict() {
    let (set, sim) = ws(&[("w0", "10.0.0.1"), ("w1", "10.0.0.2")], 5060);
    set.set_health("w0", WorkerHealth::Alive);
    set.set_health("w1", WorkerHealth::Alive);
    let mut verdicts = set.read_dead();
    sim.set_conditions("w0", false, true);
    sim.remove("w1");
    set.recompose();
    assert!(set.snapshot().is_empty(), "both left the projection");
    assert_eq!(told(&mut verdicts), Vec::<WorkerId>::new());
}

#[test]
fn a_worker_read_alive_again_after_its_return_flips_into_a_second_verdict() {
    let (set, sim) = ws(&[("w0", "10.0.0.1")], 5060);
    set.set_health("w0", WorkerHealth::Alive);
    let mut verdicts = set.read_dead();
    sim.set_conditions("w0", false, false);
    set.recompose();
    assert_eq!(told(&mut verdicts), ["w0"]);

    sim.set_conditions("w0", true, false);
    set.recompose();
    sim.set_conditions("w0", false, false);
    set.recompose();
    assert_eq!(told(&mut verdicts), Vec::<WorkerId>::new(), "unprobed since its return");

    sim.set_conditions("w0", true, false);
    set.recompose();
    set.set_health("w0", WorkerHealth::Alive);
    sim.set_conditions("w0", false, false);
    set.recompose();
    assert_eq!(told(&mut verdicts), ["w0"]);
}

#[test]
fn a_flip_read_at_a_new_host_is_the_verdict_of_the_worker_read_alive() {
    let (set, sim) = ws(&[("w0", "10.0.0.1")], 5060);
    set.set_health("w0", WorkerHealth::Alive);
    let mut verdicts = set.read_dead();
    sim.change_address(Peer::new("w0", "10.0.0.2"));
    sim.set_conditions("w0", false, false);
    set.recompose();
    assert_eq!(told(&mut verdicts), ["w0"]);
    let old = set.lookup_by_address(&ProxyAddr::new("10.0.0.1", 5060)).expect("tombstoned");
    assert_eq!(old.health, WorkerHealth::Dead);
}

#[test]
fn a_host_move_of_a_ready_worker_is_no_verdict() {
    let (set, sim) = ws(&[("w0", "10.0.0.1")], 5060);
    set.set_health("w0", WorkerHealth::Alive);
    let mut verdicts = set.read_dead();
    sim.change_address(Peer::new("w0", "10.0.0.2"));
    set.recompose();
    assert_eq!(told(&mut verdicts), Vec::<WorkerId>::new());
}

/// Two recomposes racing over one flip: only the one whose write removed the
/// worker's record tells it.
#[test]
fn a_flip_is_told_once_whichever_recompose_reads_it() {
    let (set, sim) = ws(&[("w0", "10.0.0.1")], 5060);
    set.set_health("w0", WorkerHealth::Alive);
    let set = Arc::new(set);
    let mut verdicts = set.read_dead();
    sim.set_conditions("w0", false, false);
    let runs: Vec<_> = (0..8)
        .map(|_| {
            let set = set.clone();
            std::thread::spawn(move || set.recompose())
        })
        .collect();
    for run in runs {
        run.join().expect("a recompose");
    }
    assert_eq!(told(&mut verdicts), ["w0"]);
}

#[test]
fn read_dead_now_names_the_workers_read_dead_and_the_flipped_members() {
    let (set, sim) =
        ws(&[("w0", "10.0.0.1"), ("w1", "10.0.0.2"), ("w2", "10.0.0.3"), ("w3", "10.0.0.4")], 5060);
    set.set_health("w0", WorkerHealth::Alive);
    set.set_health("w1", WorkerHealth::Dead);
    sim.set_conditions("w2", false, false);
    sim.set_conditions("w3", false, true);
    set.recompose();
    assert_eq!(set.read_dead_now(), ["w1", "w2"], "a terminating member stands on no verdict");
}

#[test]
fn preset_port_override_survives_recompose() {
    let (set, sim) = ws(&[], 5060);
    set.preset("w0", "10.0.0.1".to_string(), 5070, WorkerHealth::Alive, Acceptance::Refuse);
    sim.add(Peer::new("w0", "10.0.0.1"));
    set.recompose();
    let w0 = set.resolve("w0").unwrap();
    assert_eq!(w0.address, ProxyAddr::new("10.0.0.1", 5070), "per-worker port_override honoured");
    assert_eq!(w0.health, WorkerHealth::Alive, "preset health honoured");
    assert_eq!(w0.acceptance, Acceptance::Refuse, "preset acceptance honoured");
}

#[test]
fn acceptance_is_written_apart_from_health_and_survives_a_recompose() {
    let (set, _sim) = ws(&[("w0", "10.0.0.1")], 5060);
    assert_eq!(set.resolve("w0").unwrap().acceptance, Acceptance::Accept);
    set.set_health("w0", WorkerHealth::Alive);
    set.set_acceptance("w0", Acceptance::Refuse);
    set.recompose();
    let w0 = set.resolve("w0").unwrap();
    assert_eq!((w0.health, w0.acceptance), (WorkerHealth::Alive, Acceptance::Refuse));
}
