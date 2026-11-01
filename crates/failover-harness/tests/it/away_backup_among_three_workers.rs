//! **With three workers the proxy names an away backup only when no other
//! worker is alive** (ADR-0055 §8). One other worker alive is the call's
//! backup, as ever. With both others away, the proxy names one of them,
//! ranked by the call key among the ordinals it still knows, the same pick
//! for the same Call-ID; that one's return carries the call, and the
//! primary's drain exits on it holding the call.
//!
//! ```text
//!   one alive   b1 restarts; a call on b2 or b3 names the other alive one
//!   none alive  b1 and b3 restart; a call on b2 names HRW(Call-ID, {b1, b3});
//!               that worker returns, b2 drains ⇒ caught_up and exits, the
//!               caller's BYE ─▶ the named backup ─▶ 200
//! ```
//!
//! Every call ends with the caller's BYE answered 200 and one CDR.
//!
//! Beyond the failover sweep: the pick of an away backup among three workers.

use std::net::SocketAddr;
use std::time::Duration;

use failover_harness::{total_cdrs_for, FailoverHarness, ProxySut, ReplicatedB2buaSut};
use sip_proxy::strategies::rendezvous::{rendezvous_select, RendezvousCandidate};

use crate::away_backup_support::{
    answered_with, caller_hangs_up, come_back, drain_caught_up, exits, restart_in_place, settle,
    ALICE, BOB, PROXY,
};

const WORKERS: [(&str, &str); 3] =
    [("b1", "127.0.0.1:5091"), ("b2", "127.0.0.1:5092"), ("b3", "127.0.0.1:5093")];

/// The proxy and the three workers, read alive, ready at steady state.
async fn bring_up_three(
    fh: &mut FailoverHarness,
) -> (ProxySut, ReplicatedB2buaSut, ReplicatedB2buaSut, ReplicatedB2buaSut) {
    let pool: Vec<(&str, SocketAddr)> =
        WORKERS.iter().map(|(o, a)| (*o, a.parse().unwrap())).collect();
    let proxy = fh.spawn_proxy(PROXY, &pool).await;
    let (rt, ob) = (("127.0.0.1", 5070), ("127.0.0.1", 5080));
    let b1 = fh.spawn_worker("b1", "b1", WORKERS[0].1, &["b2", "b3"], rt, ob).await;
    let b2 = fh.spawn_worker("b2", "b2", WORKERS[1].1, &["b1", "b3"], rt, ob).await;
    let b3 = fh.spawn_worker("b3", "b3", WORKERS[2].1, &["b1", "b2"], rt, ob).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(b1.is_ready() && b2.is_ready() && b3.is_ready(), "the three workers are ready");
    (proxy, b1, b2, b3)
}

struct Ordinal(&'static str);

impl RendezvousCandidate for Ordinal {
    fn id(&self) -> &str {
        self.0
    }
}

/// The rendezvous winner for `call_id` among `ordinals`.
fn ranked_first(call_id: &str, ordinals: &[&'static str]) -> &'static str {
    let keys: Vec<Ordinal> = ordinals.iter().map(|o| Ordinal(o)).collect();
    rendezvous_select(call_id, &keys).expect("a candidate").0
}

/// A Call-ID the proxy places on `primary` while `others` alone take new
/// calls with it.
fn call_id_placed_on(primary: &'static str, alive: &[&'static str], label: &str) -> String {
    (0..)
        .map(|i| format!("{label}-{i}@127.0.0.1"))
        .find(|cid| ranked_first(cid, alive) == primary)
        .expect("some Call-ID lands on each worker")
}

#[tokio::test(start_paused = true)]
async fn with_one_other_worker_alive_it_is_the_backup() {
    let name = "three-workers-one-other-alive";
    let mut fh = FailoverHarness::new(name, &["b1", "b2", "b3"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut b1, b2, b3) = bring_up_three(&mut fh).await;

    restart_in_place(&mut fh, &mut b1);
    fh.advance(Duration::from_secs(1)).await;
    let call_id = call_id_placed_on("b2", &["b2", "b3"], "one-alive");
    let mut call = answered_with(&fh, &alice, &bob, &proxy, &b2, Some(&call_id)).await;
    assert_eq!(
        call.cookie,
        ("b2".to_string(), "b3".to_string()),
        "the other alive worker is the backup, the away one is not named"
    );
    caller_hangs_up(&mut call.dialog, &bob).await;
    let nodes = [&b1, &b2, &b3];
    settle(&fh, &nodes, &[&call.call_ref], &[&alice, &bob]).await;
    assert_eq!(total_cdrs_for(&nodes, &call.call_ref), 1, "one CDR");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

#[tokio::test(start_paused = true)]
async fn with_every_other_worker_away_the_call_key_names_one_of_them() {
    let name = "three-workers-none-alive";
    let mut fh = FailoverHarness::new(name, &["b1", "b2", "b3"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut b1, mut b2, mut b3) = bring_up_three(&mut fh).await;

    restart_in_place(&mut fh, &mut b1);
    restart_in_place(&mut fh, &mut b3);
    fh.advance(Duration::from_secs(1)).await;
    let call_id = "away-pick-1@127.0.0.1";
    let named = ranked_first(call_id, &["b1", "b3"]);
    let mut call = answered_with(&fh, &alice, &bob, &proxy, &b2, Some(call_id)).await;
    assert_eq!(
        call.cookie,
        ("b2".to_string(), named.to_string()),
        "the away worker the call key ranks first is the backup"
    );

    let (backup, other) = if named == "b1" { (&mut b1, &b3) } else { (&mut b3, &b1) };
    come_back(&mut fh, backup, &proxy).await;
    assert!(backup.scan_backed_up("b2").contains(&call.call_ref), "{named} holds the call");
    drain_caught_up(&mut fh, &b2).await;
    exits(&mut fh, &mut b2).await;
    caller_hangs_up(&mut call.dialog, &bob).await;
    let nodes = [&*backup, &b2, other];
    settle(&fh, &nodes, &[&call.call_ref], &[&alice, &bob]).await;
    assert_eq!(total_cdrs_for(&nodes, &call.call_ref), 1, "one CDR");
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
