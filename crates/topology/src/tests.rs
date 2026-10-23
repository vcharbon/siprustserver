use super::*;
use sip_clock::Clock;

fn ordinals(peers: &[Peer]) -> Vec<&str> {
    peers.iter().map(|p| p.ordinal.as_str()).collect()
}

#[tokio::test]
async fn snapshot_reflects_added_peers_in_deterministic_order() {
    let m = SimulatedMembership::with_clock(vec![], Clock::test_at(0));
    // Add out of ordinal order; snapshot must come back sorted.
    m.add(Peer::new("w2", "h2"));
    m.add(Peer::new("w0", "h0"));
    m.add(Peer::new("w1", "h1"));
    assert_eq!(ordinals(&m.snapshot()), vec!["w0", "w1", "w2"]);
}

#[tokio::test]
async fn changes_observes_added_removed_address_changed_in_order() {
    let m = SimulatedMembership::with_clock(vec![], Clock::test_at(0));
    let mut rx = m.changes();

    m.add(Peer::new("w0", "h0"));
    m.change_address(Peer::new("w0", "h0-new"));
    m.remove("w0");

    assert_eq!(rx.try_recv().unwrap(), MemberDelta::Added(Peer::new("w0", "h0")));
    assert_eq!(rx.try_recv().unwrap(), MemberDelta::AddressChanged(Peer::new("w0", "h0-new")));
    assert_eq!(rx.try_recv().unwrap(), MemberDelta::Removed("w0".to_string()));
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn no_op_mutations_emit_nothing() {
    let m = SimulatedMembership::with_clock(vec![Peer::new("w0", "h0")], Clock::test_at(0));
    let mut rx = m.changes();
    // Unchanged host → no AddressChanged.
    m.change_address(Peer::new("w0", "h0"));
    // Unknown ordinal → no delta on either path.
    m.change_address(Peer::new("ghost", "h"));
    m.remove("ghost");
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn scale_up_then_down() {
    let m = SimulatedMembership::with_clock(vec![], Clock::test_at(0));
    let mut rx = m.changes();

    m.add(Peer::new("w0", "h0"));
    m.add(Peer::new("w1", "h1"));
    m.add(Peer::new("w2", "h2"));
    m.remove("w1");

    assert_eq!(ordinals(&m.snapshot()), vec!["w0", "w2"]);

    assert_eq!(rx.try_recv().unwrap(), MemberDelta::Added(Peer::new("w0", "h0")));
    assert_eq!(rx.try_recv().unwrap(), MemberDelta::Added(Peer::new("w1", "h1")));
    assert_eq!(rx.try_recv().unwrap(), MemberDelta::Added(Peer::new("w2", "h2")));
    assert_eq!(rx.try_recv().unwrap(), MemberDelta::Removed("w1".to_string()));
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn restart_remove_then_readd_same_ordinal_new_host() {
    let m = SimulatedMembership::with_clock(vec![Peer::new("w0", "h0")], Clock::test_at(0));
    let mut rx = m.changes();

    m.remove("w0");
    m.add(Peer::new("w0", "h0-restarted"));

    // Restart is observed as Removed then Added (not AddressChanged) — the
    // ordinal genuinely left and re-joined.
    assert_eq!(rx.try_recv().unwrap(), MemberDelta::Removed("w0".to_string()));
    assert_eq!(rx.try_recv().unwrap(), MemberDelta::Added(Peer::new("w0", "h0-restarted")));
    assert_eq!(m.resolve("w0").unwrap().host, "h0-restarted");
    // Single entry — re-add replaced, not duplicated.
    assert_eq!(m.snapshot().len(), 1);
}

#[tokio::test]
async fn a_condition_flip_is_a_delta_of_its_own_and_keeps_the_peer() {
    let m = SimulatedMembership::with_clock(vec![Peer::new("w0", "h0")], Clock::test_at(0));
    let mut rx = m.changes();

    m.set_conditions("w0", false, true);
    let expected = Peer::with_conditions("w0", "h0", false, true);
    assert_eq!(rx.try_recv().unwrap(), MemberDelta::ConditionsChanged(expected.clone()));
    assert_eq!(m.snapshot(), vec![expected], "still a member, at the same host");

    // Same conditions again → nothing; unknown ordinal → nothing.
    m.set_conditions("w0", false, true);
    m.set_conditions("ghost", false, false);
    assert!(rx.try_recv().is_err());

    // Back to ready: a delta, never a Removed/Added pair.
    m.set_conditions("w0", true, false);
    assert_eq!(rx.try_recv().unwrap(), MemberDelta::ConditionsChanged(Peer::new("w0", "h0")));
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn static_membership_carries_explicit_conditions() {
    let m = StaticMembership::from_peers(vec![
        Peer::new("w0", "h0"),
        Peer::with_conditions("w1", "h1", false, true),
    ]);
    let snap = m.snapshot();
    assert!(snap[0].ready && !snap[0].terminating);
    assert!(!snap[1].ready && snap[1].terminating);
}

#[tokio::test]
async fn readd_existing_ordinal_replaces_not_duplicates() {
    let m = SimulatedMembership::with_clock(vec![Peer::new("w0", "h0")], Clock::test_at(0));
    m.add(Peer::new("w0", "h0-v2"));
    assert_eq!(m.snapshot(), vec![Peer::new("w0", "h0-v2")]);
}

#[tokio::test]
async fn static_membership_parses_and_yields_stable_snapshot() {
    let m = StaticMembership::from_string("w1@host1, w0@host0", "test").unwrap();
    let snap = m.snapshot();
    // Parsed two peers, returned sorted by ordinal regardless of input order.
    assert_eq!(snap, vec![Peer::new("w0", "host0"), Peer::new("w1", "host1")]);
    // changes() never fires but is a live receiver.
    let mut rx = m.changes();
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn static_membership_empty_string_is_empty_set() {
    assert!(StaticMembership::from_string("   ", "test").unwrap().snapshot().is_empty());
}

#[test]
fn parse_peer_list_rejects_malformed() {
    assert!(parse_peer_list("t", "noatsign").is_err());
    assert!(parse_peer_list("t", "@host").is_err());
    assert!(parse_peer_list("t", "id@").is_err());
    assert!(parse_peer_list("t", "a@h,a@h2").is_err()); // duplicate ordinal
    assert!(parse_peer_list("t", "a@h,,b@h2").is_err()); // empty entry
}

#[tokio::test]
async fn reconcile_emits_add_remove_addresschanged_and_nothing_for_unchanged() {
    let state = MembershipState::new(vec![Peer::new("w0", "h0"), Peer::new("w1", "h1")]);
    let mut rx = state.changes();

    // Desired: w0 unchanged, w1 moved hosts, w2 new, (w-implicit removed: none).
    reconcile_to_desired(
        &state,
        vec![Peer::new("w1", "h1-new"), Peer::new("w0", "h0"), Peer::new("w2", "h2")],
    );
    assert_eq!(ordinals(&state.snapshot()), vec!["w0", "w1", "w2"], "w2 joined, none removed");
    // w0 unchanged → no delta; w1 moved → AddressChanged; w2 → Added. Order:
    // removals first (none), then adds/changes in desired order (w1, w0(skip), w2).
    assert_eq!(rx.try_recv().unwrap(), MemberDelta::AddressChanged(Peer::new("w1", "h1-new")));
    assert_eq!(rx.try_recv().unwrap(), MemberDelta::Added(Peer::new("w2", "h2")));
    assert!(rx.try_recv().is_err(), "w0 unchanged emits nothing");
}

#[tokio::test]
async fn reconcile_emits_conditions_changed_for_a_same_host_flag_flip() {
    let state = MembershipState::new(vec![Peer::new("w0", "h0"), Peer::new("w1", "h1")]);
    let mut rx = state.changes();
    // w0 withdrawn from routing but still in the slice; w1 unchanged.
    reconcile_to_desired(
        &state,
        vec![Peer::with_conditions("w0", "h0", false, true), Peer::new("w1", "h1")],
    );
    assert_eq!(ordinals(&state.snapshot()), vec!["w0", "w1"], "nobody left");
    assert_eq!(
        rx.try_recv().unwrap(),
        MemberDelta::ConditionsChanged(Peer::with_conditions("w0", "h0", false, true))
    );
    assert!(rx.try_recv().is_err(), "w1 unchanged emits nothing");
    assert!(!state.resolve("w0").unwrap().ready);

    // A host move carries the new conditions whole: one AddressChanged.
    reconcile_to_desired(
        &state,
        vec![Peer::with_conditions("w0", "h0-new", true, false), Peer::new("w1", "h1")],
    );
    assert_eq!(rx.try_recv().unwrap(), MemberDelta::AddressChanged(Peer::new("w0", "h0-new")));
    assert!(rx.try_recv().is_err());
    assert!(state.resolve("w0").unwrap().ready);
}

#[tokio::test]
async fn reconcile_removes_departed_ordinals() {
    let state = MembershipState::new(vec![Peer::new("w0", "h0"), Peer::new("w1", "h1")]);
    let mut rx = state.changes();
    // w1 vanished from the desired set (pod deleted).
    reconcile_to_desired(&state, vec![Peer::new("w0", "h0")]);
    assert_eq!(ordinals(&state.snapshot()), vec!["w0"]);
    assert_eq!(rx.try_recv().unwrap(), MemberDelta::Removed("w1".to_string()));
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn reconcile_dedups_duplicate_ordinals_first_wins() {
    // Two EndpointSlices listing the same pod → one Peer, no churn.
    let state = MembershipState::new(vec![]);
    let mut rx = state.changes();
    reconcile_to_desired(&state, vec![Peer::new("w0", "10.0.0.1"), Peer::new("w0", "10.0.0.2")]);
    assert_eq!(state.snapshot(), vec![Peer::new("w0", "10.0.0.1")]);
    assert_eq!(rx.try_recv().unwrap(), MemberDelta::Added(Peer::new("w0", "10.0.0.1")));
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn reconcile_is_idempotent() {
    let state = MembershipState::new(vec![Peer::new("w0", "h0")]);
    reconcile_to_desired(&state, vec![Peer::new("w0", "h0")]);
    let mut rx = state.changes();
    // Re-applying the same desired set is a no-op (no spurious deltas).
    reconcile_to_desired(&state, vec![Peer::new("w0", "h0")]);
    assert!(rx.try_recv().is_err());
}

#[test]
fn parse_peer_list_accepts_host_with_no_port() {
    // Port-agnostic: a bare host, an FQDN, even a host:port-looking string
    // is taken whole as the host (no port split).
    let peers = parse_peer_list("t", "w0@10.0.0.1, w1@b2b.svc.cluster.local").unwrap();
    assert_eq!(peers[0].host, "10.0.0.1");
    assert_eq!(peers[1].host, "b2b.svc.cluster.local");
}
