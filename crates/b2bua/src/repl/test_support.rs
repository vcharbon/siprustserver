//! Shared scaffolding for the repl slice tests (s5–s10).
//!
//! Holds every helper the slice tests share. The only thing left local is
//! `addr(n)`: each file uses a distinct port base (9000 / 9600 / 9700 / 9800)
//! so the modules — all compiled into the one b2bua test binary — don't collide
//! on the shared simulated network. The `PullerConfig` differs per call (s5's
//! tests want the default 10 s bootstrap hard-timeout; the rest want a short
//! 2 s one), so [`supervisor_for`] takes it as a parameter rather than baking
//! one in — see [`fast_config`] and s5's local `fast_backoff`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use repl_net::transport::{ReplicationNetwork, SimulatedReplicationNetwork};
use sip_clock::Clock;
use topology::{Peer, SimulatedMembership};

use super::{Changelog, FnPeerResolver, PullerConfig, ReplServer, ReplicatingCallStore, ReplicationSupervisor};
use crate::store::{PropagateDirection, PutOpts};

/// Puller config with short backoff + a finite bootstrap hard-timeout so a
/// couple of advances trip the relevant deadline deterministically. Canonical
/// values live on [`PullerConfig::fast_test`] (shared with ha-harness).
pub fn fast_config() -> PullerConfig {
    PullerConfig::fast_test()
}

/// Advance ~`ms` in 100 ms chunks under the settle/advance/settle discipline
/// (the CLAUDE.md fake-clock hazard). Thin `ms`-typed shim over
/// [`sip_clock::testkit::pump`].
pub async fn tick(ms: u64) {
    sip_clock::testkit::pump(Duration::from_millis(ms)).await;
}

/// Forward (primary→backup) put options targeting `peer`.
pub fn fwd(peer: &str) -> PutOpts {
    PutOpts {
        peer: Some(peer.to_string()),
        direction: Some(PropagateDirection::Forward),
    }
}

/// Reverse (acting-backup→reclaiming-primary) put options targeting `peer`.
pub fn rev(peer: &str) -> PutOpts {
    PutOpts {
        peer: Some(peer.to_string()),
        direction: Some(PropagateDirection::Reverse),
    }
}

/// callRef whose encoded primary is `primary` (so `partition_of` routes it).
pub fn cref(primary: &str, id: &str) -> String {
    format!("{primary}|{id}|t{id}")
}

/// Membership of a single peer.
pub fn one_peer(ordinal: &str, clock: &Clock) -> Arc<SimulatedMembership> {
    Arc::new(SimulatedMembership::with_clock(
        vec![Peer::new(ordinal, ordinal)],
        clock.clone(),
    ))
}

/// A node = its store + changelog + listen address; the [`ReplServer`] runs in
/// the background.
///
/// The `store` field is a bare [`ReplicatingCallStore`] — it is itself
/// clone-cheap (internally `Arc`-backed), so the copy handed to the server
/// shares state with this field. Callers that need an `Arc<dyn CallStore>`
/// (e.g. to build a `CallState`) wrap a `store.clone()` at the use site; the
/// outer `Arc` is irrelevant to which state they see.
pub struct Node {
    pub store: ReplicatingCallStore,
    pub addr: SocketAddr,
}

impl Node {
    /// Build a node on `net` with incarnation `gen` and short changelog TTLs.
    pub async fn spawn(
        ordinal: &str,
        addr: SocketAddr,
        gen: u64,
        net: &Arc<SimulatedReplicationNetwork>,
        clock: &Clock,
    ) -> Self {
        let changelog = Changelog::new(gen, clock.clone()).with_ttls(30_000, 300_000);
        let store = ReplicatingCallStore::with_changelog(changelog.clone(), clock.clone());
        let listener = net.listen(addr).await.unwrap();
        let server = ReplServer::new(ordinal, changelog, Arc::new(store.clone()));
        tokio::spawn(server.run(listener));
        Self { store, addr }
    }
}

/// Wire a supervisor for `self_ordinal` to pull from a set of peers. `addrs`
/// maps each peer's ordinal to its `SocketAddr`; `config` is passed through so
/// callers pick the bootstrap timeout their scenario needs (short for most, the
/// 10 s default for s5's backoff/cold-bootstrap tests).
pub fn supervisor_for(
    self_ordinal: &str,
    store: &ReplicatingCallStore,
    net: &Arc<SimulatedReplicationNetwork>,
    clock: &Clock,
    addrs: Vec<(String, SocketAddr)>,
    config: PullerConfig,
) -> ReplicationSupervisor {
    let map: HashMap<String, SocketAddr> = addrs.into_iter().collect();
    let resolve = Arc::new(FnPeerResolver(move |peer: &Peer| *map.get(&peer.ordinal).unwrap()));
    let _ = clock; // signature kept stable for the many s5–s10 call sites
    ReplicationSupervisor::with_config(
        self_ordinal,
        net.clone(),
        store.clone(),
        resolve,
        config,
    )
}
