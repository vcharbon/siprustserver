use std::sync::Arc;
use std::time::Duration;

use super::*;

fn names(cmds: &[ReplCommand]) -> Vec<String> {
    cmds.iter()
        .map(|c| match c {
            ReplCommand::ReclaimAll => "all".to_string(),
            ReplCommand::ReclaimCall(r) => format!("call:{r}"),
            ReplCommand::ReverseFlushRefused { call_ref, .. } => format!("refused:{call_ref}"),
            other => format!("{other:?}"),
        })
        .collect()
}

fn refused(call_ref: &str) -> ReplCommand {
    ReplCommand::ReverseFlushRefused {
        call_ref: call_ref.into(),
        body: Arc::from(&b""[..]),
        origin_now_ms: 0,
    }
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
    assert!(gate.admit(refused("r")).is_some());
    assert!(released_now(&mut gate).await.is_none(), "nothing held, nothing released");
}

#[tokio::test(start_paused = true)]
async fn reclaims_wait_for_a_ready_endpoint_and_run_in_arrival_order() {
    let (tx, rx) = watch::channel(false);
    let mut gate = ReadmissionGate::new(Some(rx));
    assert!(gate.admit(ReplCommand::ReclaimAll).is_none());
    assert!(gate.admit(ReplCommand::ReclaimCall("r1".into())).is_none());
    assert!(gate.admit(ReplCommand::ReclaimAll).is_none(), "a repeated bulk reclaim is held once");
    assert!(gate.admit(refused("r1")).is_none());
    let departed = gate.admit(ReplCommand::PeerDeparted("w1".into()));
    assert!(departed.is_some(), "a command that reclaims nothing passes");
    assert!(released_now(&mut gate).await.is_none(), "closed while not ready");

    tx.send(true).unwrap();
    let released = released_now(&mut gate).await.expect("released once ready");
    assert_eq!(names(&released), ["all", "call:r1", "refused:r1"]);
    assert!(gate.admit(ReplCommand::ReclaimAll).is_some(), "open while ready");
}

#[tokio::test(start_paused = true)]
async fn an_endpoint_that_reads_not_ready_again_holds_again() {
    let (tx, rx) = watch::channel(true);
    let mut gate = ReadmissionGate::new(Some(rx));
    assert!(gate.admit(ReplCommand::ReclaimCall("r1".into())).is_some());
    tx.send(false).unwrap();
    assert!(gate.admit(ReplCommand::ReclaimCall("r2".into())).is_none(), "a flap closes it");
    assert!(released_now(&mut gate).await.is_none());
    tx.send(true).unwrap();
    let released = released_now(&mut gate).await.expect("released once ready again");
    assert_eq!(names(&released), ["call:r2"]);
}

#[tokio::test(start_paused = true)]
async fn a_command_reaching_an_open_gate_waits_behind_what_it_holds() {
    let (tx, rx) = watch::channel(false);
    let mut gate = ReadmissionGate::new(Some(rx));
    assert!(gate.admit(ReplCommand::ReclaimAll).is_none());
    tx.send(true).unwrap();
    assert!(gate.admit(ReplCommand::ReclaimCall("r".into())).is_none(), "behind the held reclaim");
    let released = released_now(&mut gate).await.expect("released");
    assert_eq!(names(&released), ["all", "call:r"]);
}
