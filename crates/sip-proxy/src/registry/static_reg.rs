//! Static worker registry — a fixed worker pool from `id@host:port,...` (the
//! `PROXY_WORKERS` grammar). Now a thin wrapper over the shared [`WorkerSet`]: the
//! identity (ordinal + host) is a [`topology::StaticMembership`]; the per-worker
//! port + health are presets in the annotation overlay. Because a static
//! membership never changes, the projection is composed once at construction; the
//! OPTIONS [`HealthProbe`](crate::health::HealthProbe) still updates health live
//! through [`control`](StaticWorkerRegistry::control).

use std::sync::Arc;

use sip_clock::Clock;
use topology::{Peer, StaticMembership};

use crate::addr::ProxyAddr;

use super::control::{WorkerRegistryControl, WorkerSetControl};
use super::projection::WorkerSet;
use super::{WorkerEntry, WorkerRegistry};

/// Fallback port for a peer with no per-worker `port_override`. Unused in
/// practice — every parsed/seeded static entry carries an explicit port — but the
/// projection needs a default.
const DEFAULT_PORT: u16 = 5060;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("static worker registry parse error ({origin}): {reason}")]
pub struct StaticRegistryParseError {
    pub origin: String,
    pub reason: String,
}

/// Parse `id@host:port,...` into `WorkerEntry`s (all `alive`). An empty/blank
/// string yields an empty set. Rejects empty entries, missing/edge `@`, empty
/// ids, duplicate ids, and malformed `host:port`.
pub fn parse_worker_list(source: &str, raw: &str) -> Result<Vec<WorkerEntry>, StaticRegistryParseError> {
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
        let at = part.find('@');
        match at {
            // `at <= 0` (no `@`, or leading `@`) or trailing `@` are invalid.
            Some(0) | None => return Err(err(format!("entry \"{part}\" must be of the form id@host:port"))),
            Some(at) if at == part.len() - 1 => {
                return Err(err(format!("entry \"{part}\" must be of the form id@host:port")))
            }
            Some(at) => {
                let id = part[..at].trim();
                if id.is_empty() {
                    return Err(err(format!("empty worker id in entry \"{part}\"")));
                }
                if !seen.insert(id.to_string()) {
                    return Err(err(format!("duplicate worker id \"{id}\"")));
                }
                let addr = ProxyAddr::parse(&part[at + 1..])
                    .filter(|a| a.port >= 1)
                    .ok_or_else(|| err(format!("entry \"{part}\" has malformed host:port (port must be 1..65535)")))?;
                out.push(WorkerEntry::alive(id, addr));
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
    /// Build from an inline `id@host:port,...` string. Fails on malformed input.
    pub fn from_string(raw: &str, source: &str) -> Result<Self, StaticRegistryParseError> {
        Ok(Self::from_entries(parse_worker_list(source, raw)?))
    }

    /// Build directly from entries (programmatic wiring/tests). The `id@host`
    /// becomes the topology membership; the `:port` + health become annotation
    /// presets so the one-shot recompose materialises them.
    pub fn from_entries(entries: Vec<WorkerEntry>) -> Self {
        let membership = StaticMembership::from_peers(
            entries.iter().map(|e| Peer::new(e.id.clone(), e.address.host.clone())).collect(),
        );
        let set = Arc::new(WorkerSet::new(Arc::new(membership), DEFAULT_PORT, Clock::system()));
        for e in &entries {
            set.preset(&e.id, e.address.host.clone(), e.address.port, e.health, e.draining_since, e.first_seen_at_ms);
        }
        set.recompose();
        Self { set }
    }

    /// A health-write [`WorkerRegistryControl`] over this pool. The OPTIONS
    /// [`HealthProbe`](crate::health::HealthProbe) writes through it so an
    /// unanswered worker is demoted (`Dead`/`NotReady`/`Draining`) and in-dialog
    /// requests fail over to the backup — the *identity* stays fixed, the *health*
    /// tracks live reachability.
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
    fn resolve(&self, id: &str) -> Option<WorkerEntry> {
        self.set.resolve(id)
    }
    fn lookup_by_address(&self, addr: &ProxyAddr) -> Option<WorkerEntry> {
        self.set.lookup_by_address(addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::WorkerHealth;

    #[test]
    fn parses_valid_list_all_alive() {
        let r = StaticWorkerRegistry::from_string("b2b-1@10.0.0.2:5070, b2b-2@10.0.0.3:5070", "test").unwrap();
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
    fn rejects_malformed_entries() {
        assert!(parse_worker_list("t", "noatsign:5070").is_err());
        assert!(parse_worker_list("t", "@10.0.0.2:5070").is_err());
        assert!(parse_worker_list("t", "id@").is_err());
        assert!(parse_worker_list("t", "id@host:notaport").is_err());
        assert!(parse_worker_list("t", "a@h:1,a@h:2").is_err()); // duplicate id
        assert!(parse_worker_list("t", "a@h:1,,b@h:2").is_err()); // empty entry
    }

    #[test]
    fn membership_identity_matches_worker_set() {
        let r = StaticWorkerRegistry::from_string("b2b-1@10.0.0.2:5070, b2b-2@10.0.0.3:5070", "test").unwrap();
        let peers = r.membership().snapshot();
        assert_eq!(peers, vec![Peer::new("b2b-1", "10.0.0.2"), Peer::new("b2b-2", "10.0.0.3")]);
    }
}
