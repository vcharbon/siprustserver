//! The SIP routing index: each resident call's [`call::index_key`] keys,
//! pointing at its `callRef`, kept as the call is written and resolved by the
//! dispatcher for a message that carries no `callRef` of ours.

use std::collections::HashMap;

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

/// Resolve `lookup` against `index`, a SIP index of key → callRef, probing in
/// [`IndexLookup::probes`] order; `key` is the buffer each probe's key is
/// written into ([`IndexLookup::key_buffer`]).
pub(crate) fn resolve_in(
    index: &HashMap<String, String>,
    lookup: &IndexLookup<'_>,
    key: &mut String,
) -> Option<IndexHit> {
    for probe in lookup.probes() {
        match *probe {
            Probe::Resolve(kind) => {
                lookup.write_key(kind, key);
                if let Some(r) = index.get(key.as_str()) {
                    return Some(IndexHit { call_ref: r.clone(), kind });
                }
            }
            Probe::Refuse(kind) => {
                lookup.write_key(kind, key);
                if index.contains_key(key.as_str()) {
                    return None;
                }
            }
        }
    }
    None
}

impl CallState {
    /// Resolve a message that carries no `callRef` of ours to a call from
    /// the in-memory index only (the sync path the dispatcher's route key
    /// needs), probing in [`IndexLookup::probes`] order. The hit names the
    /// namespace that matched.
    pub fn resolve_from_sip_key_sync(&self, lookup: IndexLookup<'_>) -> Option<IndexHit> {
        let mut key = lookup.key_buffer();
        let inner = self.locked();
        resolve_in(&inner.sip_index, &lookup, &mut key)
    }

    /// The resident calls holding `call_id` (ADR-0063 §4), each once: the
    /// incoming leg opened by each of `caller_tags`, the outgoing leg on
    /// `call_id`, and each of `call_refs` still resident. A spiral's middle
    /// Call-ID names two calls.
    pub fn calls_holding(
        &self,
        call_id: &str,
        caller_tags: &[String],
        call_refs: &[String],
    ) -> Vec<String> {
        let leg = IndexLookup::Leg { call_id };
        let callers = caller_tags.iter().map(|from_tag| IndexLookup::Invite { call_id, from_tag });
        let mut key = String::new();
        let inner = self.locked();
        let indexed = std::iter::once(leg)
            .chain(callers)
            .filter_map(|lookup| resolve_in(&inner.sip_index, &lookup, &mut key))
            .map(|hit| hit.call_ref);
        let resident = call_refs.iter().filter(|r| inner.calls.contains_key(r.as_str())).cloned();
        let mut found: Vec<String> = Vec::new();
        for call_ref in indexed.chain(resident) {
            if !found.contains(&call_ref) {
                found.push(call_ref);
            }
        }
        found
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
