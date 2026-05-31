//! Static worker registry — port of `registry/static.ts`. Parses
//! `id@host:port,id@host:port,...` into an immutable set of `alive` workers
//! (dev/local wiring + tests). No dynamic membership: `changes()` never fires.
//!
//! **Membership identity (ordinal + host) is sourced from the `topology`
//! crate** (S1b): the `id@host` portion of each entry is fed into a
//! [`topology::StaticMembership`], which is the single source of truth for
//! *who is in the cluster*. The proxy layers its own concerns — the transport
//! `:port` (kept in [`ProxyAddr`]) and `Alive` health — on top of that
//! membership to materialise the richer [`WorkerEntry`] view.

use std::collections::HashMap;

use topology::{Peer, StaticMembership};

use crate::addr::ProxyAddr;

use super::{RegistryState, WorkerEntry, WorkerRegistry};

/// Layer-build-time parse failure.
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

/// A fixed worker set. `snapshot`/`resolve`/`lookup_by_address` are lock-free
/// reads; `changes()` is an empty (never-firing) subscription.
///
/// Cluster *membership identity* lives in the embedded
/// [`topology::StaticMembership`] (`ordinal@host`); the proxy's [`RegistryState`]
/// is the outward, port-and-health-annotated materialisation of that membership.
/// Because a static membership never changes, the two are built once at
/// construction and stay in lock-step by construction.
pub struct StaticWorkerRegistry {
    /// Membership identity source of truth (ordinal + host, port-agnostic).
    membership: StaticMembership,
    /// Port-and-health-annotated materialisation read on the hot path.
    state: RegistryState,
}

impl StaticWorkerRegistry {
    /// Build from an inline `id@host:port,...` string (the `PROXY_WORKERS`
    /// grammar). Fails on malformed input.
    pub fn from_string(raw: &str, source: &str) -> Result<Self, StaticRegistryParseError> {
        Ok(Self::from_entries(parse_worker_list(source, raw)?))
    }

    /// Build directly from entries (programmatic wiring/tests).
    pub fn from_entries(entries: Vec<WorkerEntry>) -> Self {
        // Derive the membership identity (ordinal + host) from the entries: the
        // `:port` and health stay in the proxy's `WorkerEntry`, the `id@host`
        // becomes the topology peer set (the membership source of truth).
        let membership = StaticMembership::from_peers(
            entries.iter().map(|e| Peer::new(e.id.clone(), e.address.host.clone())).collect(),
        );
        let materialised = materialise(&membership, &entries);
        Self { membership, state: RegistryState::new(materialised) }
    }

    /// The cluster membership identity backing this registry (ordinal + host).
    /// Exposed so future HA wiring (S11 k8s watcher / b2bua replication) can read
    /// the same membership the proxy routes over.
    pub fn membership(&self) -> &StaticMembership {
        &self.membership
    }
}

/// Materialise the proxy's `WorkerEntry` view from the topology membership
/// (identity + host) plus the proxy's per-id annotations (port + health, carried
/// in `entries`). The membership snapshot is the authoritative who-and-where;
/// the port/health come from the originally-parsed entries keyed by ordinal.
fn materialise(membership: &topology::StaticMembership, entries: &[WorkerEntry]) -> Vec<WorkerEntry> {
    use topology::Membership;
    let by_id: HashMap<&str, &WorkerEntry> = entries.iter().map(|e| (e.id.as_str(), e)).collect();
    membership
        .snapshot()
        .into_iter()
        .filter_map(|peer| {
            // Carry the proxy's port + health for this ordinal; membership owns
            // the host (so an AddressChanged would flow through here too).
            by_id.get(peer.ordinal.as_str()).map(|orig| WorkerEntry {
                id: peer.ordinal,
                address: ProxyAddr::new(peer.host, orig.address.port),
                health: orig.health,
                draining_since: orig.draining_since,
                first_seen_at_ms: orig.first_seen_at_ms,
            })
        })
        .collect()
}

impl WorkerRegistry for StaticWorkerRegistry {
    fn snapshot(&self) -> Vec<WorkerEntry> {
        self.state.snapshot()
    }
    fn resolve(&self, id: &str) -> Option<WorkerEntry> {
        self.state.resolve(id)
    }
    fn lookup_by_address(&self, addr: &ProxyAddr) -> Option<WorkerEntry> {
        self.state.lookup_by_address(addr)
    }
    fn changes(&self) -> tokio::sync::broadcast::Receiver<super::RegistryEvent> {
        self.state.changes()
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
        use topology::Membership;
        let r = StaticWorkerRegistry::from_string("b2b-1@10.0.0.2:5070, b2b-2@10.0.0.3:5070", "test").unwrap();
        // The topology membership is the identity source: same ordinals + hosts,
        // port-agnostic.
        let peers = r.membership().snapshot();
        assert_eq!(peers, vec![Peer::new("b2b-1", "10.0.0.2"), Peer::new("b2b-2", "10.0.0.3")]);
    }
}
