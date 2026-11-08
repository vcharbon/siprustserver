//! One reaper ledger: a value per callRef, each entry stamped afresh at
//! every write. A prune lists the entries under the ledger's lock, judges
//! them after it, and removes under it again only the entries no write
//! touched since the listing.

use std::collections::HashMap;

/// A ledger entry's stamp: unique within its ledger, never reused.
pub(super) type Stamp = u64;

/// Per-callRef entries with their stamps.
#[derive(Default)]
pub(super) struct Ledger<V> {
    entries: HashMap<String, (Stamp, V)>,
    last: Stamp,
}

impl<V: Default> Ledger<V> {
    /// Write `call_ref`'s entry, created at `V::default()` when absent:
    /// its value to update, and whether it is new. The entry takes a fresh
    /// stamp either way.
    pub(super) fn write(&mut self, call_ref: &str) -> (&mut V, bool) {
        self.last += 1;
        let stamp = self.last;
        let mut new = false;
        let entry = self.entries.entry(call_ref.to_string()).or_insert_with(|| {
            new = true;
            (stamp, V::default())
        });
        entry.0 = stamp;
        (&mut entry.1, new)
    }

    /// Whether `call_ref` has an entry.
    pub(super) fn contains(&self, call_ref: &str) -> bool {
        self.entries.contains_key(call_ref)
    }

    /// Remove `call_ref`'s entry; whether it had one.
    pub(super) fn remove(&mut self, call_ref: &str) -> bool {
        self.entries.remove(call_ref).is_some()
    }

    /// Every entry's callRef and stamp, to judge outside the lock.
    pub(super) fn listed(&self) -> Vec<(String, Stamp)> {
        self.entries.iter().map(|(call_ref, (stamp, _))| (call_ref.clone(), *stamp)).collect()
    }

    /// Remove the `judged` entries that still carry their listed stamp: an
    /// entry written since its listing stays.
    pub(super) fn prune(&mut self, judged: Vec<(String, Stamp)>) {
        for (call_ref, stamp) in judged {
            if self.entries.get(&call_ref).is_some_and(|(now, _)| *now == stamp) {
                self.entries.remove(&call_ref);
            }
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A prune removes the listed entries left as they were, and keeps the
    /// ones written, re-created or created since the listing.
    #[test]
    fn a_prune_removes_only_entries_untouched_since_the_listing() {
        let mut ledger: Ledger<u32> = Ledger::default();
        for call_ref in ["kept-as-is", "rewritten", "re-created"] {
            *ledger.write(call_ref).0 += 1;
        }
        let listed = ledger.listed();
        *ledger.write("rewritten").0 += 1;
        assert!(ledger.remove("re-created"));
        let (value, new) = ledger.write("re-created");
        assert!(new);
        *value += 1;
        ledger.write("created");

        ledger.prune(listed);
        assert!(!ledger.contains("kept-as-is"));
        assert_eq!(ledger.entries.get("rewritten").map(|e| e.1), Some(2));
        assert_eq!(ledger.entries.get("re-created").map(|e| e.1), Some(1));
        assert!(ledger.contains("created"));
        assert_eq!(ledger.len(), 3);
    }

    /// A write of a present entry is no new entry, and keeps its value.
    #[test]
    fn a_rewrite_keeps_the_entry() {
        let mut ledger: Ledger<()> = Ledger::default();
        assert!(ledger.write("a").1);
        let listed = ledger.listed();
        assert!(!ledger.write("a").1);
        ledger.prune(listed);
        assert!(ledger.contains("a"));
    }
}
