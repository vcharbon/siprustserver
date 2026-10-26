//! One independently locked stripe of the branch memo: a table of entries
//! with an index ordered by expiry, so an expiry pass and an eviction reach
//! the entry nearest its expiry without looking at any other.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use super::bounds::footprint;
use super::CancelEntry;

/// An entry's place in the expiry index: its expiry, then the stripe's write
/// sequence, which keeps two entries expiring together apart.
type ExpiryKey = (u64, u64);

struct Slot {
    entry: CancelEntry,
    at: ExpiryKey,
    bytes: usize,
}

/// What an operation on a stripe changed, for the memo's totals.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Change {
    pub added: usize,
    pub removed: usize,
    pub bytes_added: usize,
    pub bytes_removed: usize,
    /// Entries forgotten before their expiry, the stripe being past a bound.
    pub evicted: usize,
    /// Expiry-index entries an expiry pass looked at.
    pub inspected: usize,
}

/// One stripe's entries, held to at most `max_entries` entries and
/// `max_bytes` accounted bytes ([`footprint`]).
pub(super) struct Stripe {
    table: HashMap<Arc<str>, Slot>,
    by_expiry: BTreeMap<ExpiryKey, Arc<str>>,
    bytes: usize,
    next_seq: u64,
    max_entries: usize,
    max_bytes: usize,
}

impl Stripe {
    pub(super) fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            table: HashMap::new(),
            by_expiry: BTreeMap::new(),
            bytes: 0,
            next_seq: 0,
            max_entries,
            max_bytes,
        }
    }

    /// Forget every entry expired at `now_ms`: the expiry index is walked
    /// from its front and the walk ends at the first entry still live.
    pub(super) fn expire(&mut self, now_ms: u64, change: &mut Change) {
        while let Some((&at, _)) = self.by_expiry.first_key_value() {
            change.inspected = change.inspected.saturating_add(1);
            if at.0 > now_ms {
                break;
            }
            self.remove_at(at, change);
        }
    }

    /// Write `entry` under `key`, expiring at `expires_at_ms`; a key already
    /// held keeps one entry, the new one. Past a bound the entries nearest
    /// their expiry are forgotten first, the new one included when it is.
    pub(super) fn insert(
        &mut self,
        key: &str,
        entry: CancelEntry,
        expires_at_ms: u64,
        change: &mut Change,
    ) {
        let at = (expires_at_ms, self.next_seq);
        self.next_seq = self.next_seq.wrapping_add(1);
        let bytes = footprint(key, &entry);
        let shared = match self.table.get_key_value(key) {
            Some((shared, slot)) => {
                let (shared, old_at) = (shared.clone(), slot.at);
                self.remove_at(old_at, change);
                shared
            }
            None => Arc::from(key),
        };
        self.by_expiry.insert(at, shared.clone());
        self.table.insert(shared, Slot { entry, at, bytes });
        self.bytes = self.bytes.saturating_add(bytes);
        change.added = change.added.saturating_add(1);
        change.bytes_added = change.bytes_added.saturating_add(bytes);
        while self.table.len() > self.max_entries || self.bytes > self.max_bytes {
            let Some((&front, _)) = self.by_expiry.first_key_value() else { break };
            self.remove_at(front, change);
            change.evicted = change.evicted.saturating_add(1);
        }
    }

    /// The entry under `key` while it is live at `now_ms`; an expired one is
    /// forgotten here.
    pub(super) fn get(
        &mut self,
        key: &str,
        now_ms: u64,
        change: &mut Change,
    ) -> Option<CancelEntry> {
        let slot = self.table.get(key)?;
        if slot.at.0 > now_ms {
            return Some(slot.entry.clone());
        }
        let at = slot.at;
        self.remove_at(at, change);
        None
    }

    fn remove_at(&mut self, at: ExpiryKey, change: &mut Change) {
        let Some(key) = self.by_expiry.remove(&at) else { return };
        let Some(slot) = self.table.remove(&key) else { return };
        self.bytes = self.bytes.saturating_sub(slot.bytes);
        change.removed = change.removed.saturating_add(1);
        change.bytes_removed = change.bytes_removed.saturating_add(slot.bytes);
    }
}
