//! Static worker registry — port of `registry/static.ts`. Parses
//! `id@host:port,id@host:port,...` into an immutable set of `alive` workers
//! (dev/local wiring + tests). No dynamic membership: `changes()` never fires.

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
pub struct StaticWorkerRegistry {
    state: RegistryState,
}

impl StaticWorkerRegistry {
    /// Build from an inline `id@host:port,...` string (the `PROXY_WORKERS`
    /// grammar). Fails on malformed input.
    pub fn from_string(raw: &str, source: &str) -> Result<Self, StaticRegistryParseError> {
        Ok(Self { state: RegistryState::new(parse_worker_list(source, raw)?) })
    }

    /// Build directly from entries (programmatic wiring/tests).
    pub fn from_entries(entries: Vec<WorkerEntry>) -> Self {
        Self { state: RegistryState::new(entries) }
    }
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
}
