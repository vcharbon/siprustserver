//! Static worker registry: a fixed worker pool from `id@host:port,...` (the
//! `PROXY_WORKERS` grammar) over the shared `WorkerSet`. The identity (ordinal
//! and host) is a [`topology::StaticMembership`]; each worker's port and health
//! are presets in the annotation overlay. The membership never changes, so the
//! projection is composed once at construction, and the OPTIONS
//! [`HealthProbe`](crate::health::HealthProbe) updates health live through
//! [`control`](StaticWorkerRegistry::control).

use std::sync::Arc;

use sip_clock::Clock;
use topology::{Peer, StaticMembership};

use crate::addr::ProxyAddr;

use super::control::{WorkerRegistryControl, WorkerSetControl};
use super::projection::WorkerSet;
use super::{WorkerEntry, WorkerRegistry};

/// The projection's default port. Every static entry presets its own port, so
/// no static worker takes this one.
const DEFAULT_PORT: u16 = 5060;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("static worker registry parse error ({origin}): {reason}")]
pub struct StaticRegistryParseError {
    pub origin: String,
    pub reason: String,
}

/// Parses `id@host:port,...` into `WorkerEntry`s, all `alive`. A blank string
/// yields an empty set. Refuses an empty entry, a missing, leading or trailing
/// `@`, an empty or duplicate id, a malformed `host:port` or port 0 (an IPv6
/// host bracketed), and a host that is no IP literal: `lookup_by_address`
/// compares host strings against datagram source addresses, so a named worker
/// would never match. A host is held in its canonical text, as a source
/// address prints.
pub fn parse_worker_list(
    source: &str,
    raw: &str,
) -> Result<Vec<WorkerEntry>, StaticRegistryParseError> {
    let err = |reason: String| StaticRegistryParseError { origin: source.to_string(), reason };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for part_raw in trimmed.split(',') {
        let part = part_raw.trim();
        if part.is_empty() {
            return Err(err(format!("empty entry in {source}")));
        }
        match part.split_once('@') {
            None | Some(("", _)) | Some((_, "")) => {
                return Err(err(format!("entry \"{part}\" must be of the form id@host:port")))
            }
            Some((id, host_port)) => {
                let id = id.trim();
                if id.is_empty() {
                    return Err(err(format!("empty worker id in entry \"{part}\"")));
                }
                if !seen.insert(id.to_string()) {
                    return Err(err(format!("duplicate worker id \"{id}\"")));
                }
                let addr = ProxyAddr::parse(host_port).ok_or_else(|| {
                    err(format!(
                        "entry \"{part}\" has malformed host:port (port must be 1..65535, an \
                         IPv6 host bracketed)"
                    ))
                })?;
                let Ok(ip) = addr.host.parse::<std::net::IpAddr>() else {
                    return Err(err(format!(
                        "entry \"{part}\": worker host must be an IP literal — a DNS name breaks \
                         worker-outbound classification (lookup_by_address compares raw host strings)"
                    )));
                };
                let canonical = std::net::SocketAddr::new(ip, addr.port);
                out.push(WorkerEntry::alive(id, ProxyAddr::from(canonical)));
            }
        }
    }
    Ok(out)
}

/// A fixed worker pool. `snapshot`/`resolve`/`lookup_by_address` are lock-free
/// projection reads; health tracks live OPTIONS reachability via `control`.
pub struct StaticWorkerRegistry {
    set: Arc<WorkerSet>,
}

impl StaticWorkerRegistry {
    /// Build from an inline `id@host:port,...` string on the system clock.
    /// Fails on malformed input.
    pub fn from_string(raw: &str, source: &str) -> Result<Self, StaticRegistryParseError> {
        Self::from_string_with_clock(raw, source, Clock::system())
    }

    pub fn from_string_with_clock(
        raw: &str,
        source: &str,
        clock: Clock,
    ) -> Result<Self, StaticRegistryParseError> {
        Ok(Self::from_entries_with_clock(parse_worker_list(source, raw)?, clock))
    }

    /// Build directly from entries on the system clock (programmatic wiring /
    /// tests with no time dependency).
    pub fn from_entries(entries: Vec<WorkerEntry>) -> Self {
        Self::from_entries_with_clock(entries, Clock::system())
    }

    /// Build directly from entries with an injected clock — paused-clock tests
    /// use this (`Clock::test_at(..)`): the `WorkerSet`'s clock stamps the
    /// departed-address windows, and a hard-coded system clock breaks those
    /// under `tokio::time::advance`. The `id@host` becomes the topology
    /// membership; the `:port`, health and acceptance become annotation presets
    /// so the one-shot recompose materialises them.
    pub fn from_entries_with_clock(entries: Vec<WorkerEntry>, clock: Clock) -> Self {
        let membership = StaticMembership::from_peers(
            entries.iter().map(|e| Peer::new(e.id.clone(), e.address.host.clone())).collect(),
        );
        let set = Arc::new(WorkerSet::new(Arc::new(membership), DEFAULT_PORT, clock));
        for e in &entries {
            set.preset(&e.id, e.address.host.clone(), e.address.port, e.health, e.acceptance);
        }
        set.recompose();
        Self { set }
    }

    /// A health-write [`WorkerRegistryControl`] over this pool. The OPTIONS
    /// [`HealthProbe`](crate::health::HealthProbe) writes through it so an
    /// unanswered worker is demoted `Dead` and in-dialog requests fail over to
    /// the backup — the *identity* stays fixed, the *health* tracks live
    /// reachability.
    pub fn control(&self) -> Arc<dyn WorkerRegistryControl> {
        Arc::new(WorkerSetControl::new(self.set.clone()))
    }

    /// The cluster membership identity backing this registry (ordinal + host).
    /// Exposed so HA wiring can read the same membership the proxy routes over.
    pub fn membership(&self) -> &Arc<dyn topology::Membership> {
        self.set.membership()
    }
}

impl WorkerRegistry for StaticWorkerRegistry {
    fn snapshot(&self) -> Vec<WorkerEntry> {
        self.set.snapshot()
    }
    fn probed(&self) -> Vec<WorkerEntry> {
        self.set.probed()
    }
    fn routes_on_answers(&self) -> bool {
        self.set.routes_on_answers()
    }
    fn resolve(&self, id: &str) -> Option<WorkerEntry> {
        self.set.resolve(id)
    }
    fn lookup_by_address(&self, addr: &ProxyAddr) -> Option<WorkerEntry> {
        self.set.lookup_by_address(addr)
    }
    fn away_ordinals(&self) -> Vec<super::WorkerId> {
        self.set.away_ordinals()
    }
    fn rearm_departed(&self, addr: &ProxyAddr) {
        self.set.rearm_departed(addr)
    }
    fn synced(&self) -> bool {
        self.set.synced()
    }
    fn read_dead(&self) -> tokio::sync::broadcast::Receiver<super::WorkerId> {
        self.set.read_dead()
    }
    fn read_dead_now(&self) -> Vec<super::WorkerId> {
        self.set.read_dead_now()
    }
    fn read_recovered(&self) -> tokio::sync::broadcast::Receiver<super::WorkerId> {
        self.set.read_recovered()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::WorkerHealth;

    #[test]
    fn parses_valid_list_all_alive() {
        let r =
            StaticWorkerRegistry::from_string("b2b-1@10.0.0.2:5070, b2b-2@10.0.0.3:5070", "test")
                .unwrap();
        let snap = r.snapshot();
        assert_eq!(snap.len(), 2);
        assert!(snap.iter().all(|w| w.health == WorkerHealth::Alive));
        assert_eq!(r.resolve("b2b-1").unwrap().address, ProxyAddr::new("10.0.0.2", 5070));
        assert!(r.lookup_by_address(&ProxyAddr::new("10.0.0.3", 5070)).is_some());
        assert!(r.resolve("nope").is_none());
    }

    #[test]
    fn empty_string_is_empty_set() {
        assert!(StaticWorkerRegistry::from_string("   ", "test").unwrap().snapshot().is_empty());
    }

    #[test]
    fn rejects_dns_named_hosts() {
        let e = parse_worker_list("t", "w0@b2bua-worker-0.svc:5070").unwrap_err();
        assert!(e.reason.contains("IP literal"), "got: {}", e.reason);
    }

    #[test]
    fn rejects_malformed_entries() {
        assert!(parse_worker_list("t", "noatsign:5070").is_err());
        assert!(parse_worker_list("t", "@10.0.0.2:5070").is_err());
        assert!(parse_worker_list("t", "id@").is_err());
        assert!(parse_worker_list("t", "id@host:notaport").is_err());
        assert!(parse_worker_list("t", "a@h:1,a@h:2").is_err());
        assert!(parse_worker_list("t", "a@h:1,,b@h:2").is_err());
    }

    /// An IPv6 worker is written bracketed (RFC 3261 `hostport`) and held as
    /// the bare canonical address a datagram's source prints; the unbracketed
    /// form is refused, its port being no part of it a reader can tell.
    #[test]
    fn an_ipv6_worker_is_read_bracketed_only() {
        let r = StaticWorkerRegistry::from_string("w@[2001:DB8:0::7]:5060", "test").unwrap();
        let src: std::net::SocketAddr = "[2001:db8::7]:5060".parse().unwrap();
        assert_eq!(r.resolve("w").unwrap().address, ProxyAddr::from(src));
        assert!(r.lookup_by_address(&ProxyAddr::from(src)).is_some());
        assert!(parse_worker_list("t", "w@2001:db8::7:5060").is_err());
    }

    #[test]
    fn membership_identity_matches_worker_set() {
        let r =
            StaticWorkerRegistry::from_string("b2b-1@10.0.0.2:5070, b2b-2@10.0.0.3:5070", "test")
                .unwrap();
        let peers = r.membership().snapshot();
        assert_eq!(peers, vec![Peer::new("b2b-1", "10.0.0.2"), Peer::new("b2b-2", "10.0.0.3")]);
    }
}
