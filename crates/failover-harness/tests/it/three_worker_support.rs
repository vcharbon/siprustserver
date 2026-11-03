//! What the three-worker cells share: the cluster behind the front proxy,
//! the rendezvous pick of a Call-ID's primary among the workers taking new
//! calls.

use std::net::SocketAddr;
use std::time::Duration;

use failover_harness::{FailoverHarness, ProxySut, ReplicatedB2buaSut, WorkerHealth};
use sip_proxy::strategies::rendezvous::{rendezvous_select, RendezvousCandidate};

use crate::away_backup_support::{drain, GRACE, PROXY};

pub(crate) const WORKERS: [(&str, &str); 3] =
    [("b1", "127.0.0.1:5091"), ("b2", "127.0.0.1:5092"), ("b3", "127.0.0.1:5093")];

/// The proxy and the three workers, read alive, ready at steady state.
pub(crate) async fn bring_up_three(
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
pub(crate) fn ranked_first(call_id: &str, ordinals: &[&'static str]) -> &'static str {
    let keys: Vec<Ordinal> = ordinals.iter().map(|o| Ordinal(o)).collect();
    rendezvous_select(call_id, &keys).expect("a candidate").0
}

/// A Call-ID the proxy places on `primary` while the workers `alive` alone
/// take new calls.
pub(crate) fn call_id_placed_on(primary: &str, alive: &[&'static str], label: &str) -> String {
    (0..)
        .map(|i| format!("{label}-{i}@127.0.0.1"))
        .find(|cid| ranked_first(cid, alive) == primary)
        .expect("some Call-ID lands on each worker")
}

/// A Call-ID the proxy places on `primary` and backs up on `backup` while
/// the workers `alive` alone take new calls: the first two of its rendezvous
/// ranking.
pub(crate) fn call_id_owned_by(
    primary: &str,
    backup: &str,
    alive: &[&'static str],
    label: &str,
) -> String {
    let others: Vec<&'static str> = alive.iter().copied().filter(|o| *o != primary).collect();
    (0..)
        .map(|i| format!("{label}-{i}@127.0.0.1"))
        .find(|cid| ranked_first(cid, alive) == primary && ranked_first(cid, &others) == backup)
        .expect("some Call-ID lands on each pair of workers")
}

/// `node` restarts in a fresh incarnation at a new address, listed in the
/// slice not ready: its peers pull it and bind its new identity, the proxy
/// routes nothing to it. Returns its new address.
pub(crate) async fn returns_listed_not_ready(
    fh: &mut FailoverHarness,
    node: &mut ReplicatedB2buaSut,
) -> SocketAddr {
    let ordinal = node.ordinal().to_string();
    fh.mark(&ordinal, None, "reboot", "a fresh incarnation at a new address, not ready");
    let addr = node.reboot().await;
    fh.note_worker_rebound(&ordinal, addr);
    fh.readmit(&ordinal, addr);
    fh.flap_not_ready(&ordinal);
    addr
}

/// Pump until `node` reads ready, then its endpoint reads ready and the
/// proxy reads it alive.
pub(crate) async fn published_once_ready(
    fh: &mut FailoverHarness,
    node: &ReplicatedB2buaSut,
    proxy: &ProxySut,
) {
    let ready = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || node.is_ready())
        .await;
    assert!(ready, "{} came back ready", node.ordinal());
    fh.flap_ready(node.ordinal());
    proxy.set_health(node.ordinal(), WorkerHealth::Alive);
    fh.advance(Duration::from_millis(200)).await;
}

/// [`drain`], which must exit clean (quiescent or caught up) inside the
/// grace.
pub(crate) async fn drain_clean(fh: &mut FailoverHarness, node: &ReplicatedB2buaSut) {
    let out = drain(fh, node).await;
    assert!(out.exit.is_clean(), "{}'s drain exits clean, got {out:?}", node.ordinal());
    assert!(out.elapsed < GRACE, "and inside the grace, got {:?}", out.elapsed);
}
