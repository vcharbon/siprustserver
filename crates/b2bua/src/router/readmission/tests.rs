use std::time::Duration;

use super::*;
use crate::repl::WithdrawalCondition;

fn names(cmds: &[ReplCommand]) -> Vec<String> {
    cmds.iter()
        .map(|c| match c {
            ReplCommand::ReclaimAll => "all".to_string(),
            ReplCommand::ReclaimCall(r) => format!("call:{r}"),
            other => format!("{other:?}"),
        })
        .collect()
}

/// Whether `released` resolves within a paused-clock second.
async fn released_now(gate: &mut ReadmissionGate) -> Option<Vec<ReplCommand>> {
    tokio::time::timeout(Duration::from_secs(1), gate.released()).await.ok()
}

#[tokio::test(start_paused = true)]
async fn a_membership_that_does_not_show_the_node_holds_nothing() {
    let mut gate = ReadmissionGate::new(None);
    assert!(gate.admit(ReplCommand::ReclaimAll).is_some());
    assert!(gate.admit(ReplCommand::ReclaimCall("r".into())).is_some());
    assert!(released_now(&mut gate).await.is_none(), "nothing held, nothing released");
}

#[tokio::test(start_paused = true)]
async fn reclaims_wait_for_the_routable_sighting_and_run_in_arrival_order() {
    let (tx, rx) = watch::channel(SelfEndpoint::Unobserved);
    let mut gate = ReadmissionGate::new(Some(rx));
    assert!(gate.admit(ReplCommand::ReclaimAll).is_none());
    assert!(gate.admit(ReplCommand::ReclaimCall("r1".into())).is_none());
    assert!(gate.admit(ReplCommand::ReclaimAll).is_none(), "a repeated bulk reclaim is held once");
    let departed = gate.admit(ReplCommand::PeerDeparted("w1".into()));
    assert!(departed.is_some(), "a command that reclaims nothing passes");
    assert!(released_now(&mut gate).await.is_none(), "closed while unobserved");

    tx.send(SelfEndpoint::Routable).unwrap();
    let released = released_now(&mut gate).await.expect("released on the routable sighting");
    assert_eq!(names(&released), ["all", "call:r1"]);
    assert!(gate.admit(ReplCommand::ReclaimAll).is_some(), "open from then on");

    tx.send(SelfEndpoint::Withdrawn(WithdrawalCondition::Absent)).unwrap();
    assert!(
        gate.admit(ReplCommand::ReclaimCall("r2".into())).is_some(),
        "a withdrawal closes nothing"
    );
}

#[tokio::test(start_paused = true)]
async fn a_node_superseded_before_it_was_routable_reclaims_nothing() {
    let (tx, rx) = watch::channel(SelfEndpoint::Unobserved);
    let mut gate = ReadmissionGate::new(Some(rx));
    assert!(gate.admit(ReplCommand::ReclaimAll).is_none());
    tx.send(SelfEndpoint::Superseded).unwrap();
    assert!(released_now(&mut gate).await.is_none());
    assert!(gate.admit(ReplCommand::ReclaimCall("r".into())).is_none());
}

#[tokio::test(start_paused = true)]
async fn a_gate_built_over_a_routable_endpoint_is_open() {
    let (_tx, rx) = watch::channel(SelfEndpoint::Routable);
    let mut gate = ReadmissionGate::new(Some(rx));
    assert!(gate.admit(ReplCommand::ReclaimAll).is_some());
}
