//! The SIP routing index: each resident call's [`call::index_key`] keys,
//! pointing at its `callRef`, kept as the call is written and resolved by the
//! dispatcher for a message that carries no `callRef` of ours.

use call::{IndexHit, IndexLookup, Probe};

use super::{CallState, Inner};

impl Inner {
    /// Drop `call_ref`'s index `keys`, leaving any key another call now holds.
    pub(super) fn unindex(&mut self, call_ref: &str, keys: &[String]) {
        for k in keys {
            if self.sip_index.get(k).is_some_and(|owner| owner == call_ref) {
                self.sip_index.remove(k);
            }
        }
    }
}

impl CallState {
    /// Resolve a message that carries no `callRef` of ours to a call from
    /// the in-memory index only (the sync path the dispatcher's route key
    /// needs), probing in [`IndexLookup::probes`] order. The hit names the
    /// namespace that matched.
    pub fn resolve_from_sip_key_sync(&self, lookup: IndexLookup<'_>) -> Option<IndexHit> {
        let mut key = lookup.key_buffer();
        let inner = self.locked();
        for probe in lookup.probes() {
            match *probe {
                Probe::Resolve(kind) => {
                    lookup.write_key(kind, &mut key);
                    if let Some(r) = inner.sip_index.get(key.as_str()) {
                        return Some(IndexHit { call_ref: r.clone(), kind });
                    }
                }
                Probe::Refuse(kind) => {
                    lookup.write_key(kind, &mut key);
                    if inner.sip_index.contains_key(key.as_str()) {
                        return None;
                    }
                }
            }
        }
        None
    }

    /// Point every one of `keys` at `call_ref` and make them the keys it
    /// owns, dropping any stale ones. Unchanged keys are only re-pointed
    /// where another call took them over.
    pub(super) fn reindex(inner: &mut Inner, call_ref: &str, keys: Vec<String>) {
        if inner.indexed.get(call_ref) == Some(&keys) {
            for k in &keys {
                if inner.sip_index.get(k).is_none_or(|owner| owner != call_ref) {
                    inner.sip_index.insert(k.clone(), call_ref.to_string());
                }
            }
            return;
        }
        if let Some(old) = inner.indexed.remove(call_ref) {
            inner.unindex(call_ref, &old);
        }
        for k in &keys {
            inner.sip_index.insert(k.clone(), call_ref.to_string());
        }
        inner.indexed.insert(call_ref.to_string(), keys);
    }
}
