//! One lifetime class's entries within a stripe: a table bounded in entries
//! and accounted bytes, with an index in write order. Every entry of a pool
//! lives the same time, so write order is expiry order: an expiry pass and an
//! eviction take entries from the front of the index and look at no other.

use std::collections::{BTreeMap, HashMap};
use std::mem::size_of;
use std::sync::Arc;

use super::footprint::footprint;
use super::CancelEntry;

struct Slot {
    entry: CancelEntry,
    seq: u64,
    expires_at_ms: u64,
    bytes: usize,
}

/// The bytes one entry costs this pool's two indexes: its share of the
/// table's buckets at the lowest load a table reaches after growing (7/16),
/// and its share of the write-order index's leaf at the lowest occupancy a
/// B-tree node keeps (5 of 11 cells).
pub(super) const INDEX_BYTES_PER_ENTRY: usize = (size_of::<(Arc<str>, Slot)>() + 1) * 16 / 7
    + (2 * size_of::<usize>() + 11 * (size_of::<u64>() + size_of::<Arc<str>>())) / 5;

/// What an operation on a pool changed, for the memo's totals.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Change {
    pub added: usize,
    pub removed: usize,
    pub bytes_added: usize,
    pub bytes_removed: usize,
    /// Entries forgotten before their expiry, the pool being past a bound.
    pub evicted: usize,
    /// Write-order index entries an expiry pass looked at.
    pub inspected: usize,
}

/// One lifetime class's entries, held to at most `max_entries` entries and
/// `max_bytes` accounted bytes ([`footprint`]); past a bound the oldest write
/// goes first.
pub(super) struct Pool {
    table: HashMap<Arc<str>, Slot>,
    by_write: BTreeMap<u64, Arc<str>>,
    bytes: usize,
    next_seq: u64,
    max_entries: usize,
    max_bytes: usize,
}

impl Pool {
    pub(super) fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            table: HashMap::new(),
            by_write: BTreeMap::new(),
            bytes: 0,
            next_seq: 0,
            max_entries,
            max_bytes,
        }
    }

    /// Forget every entry expired at `now_ms`, walking the write order from
    /// its front; the walk ends at the first entry still live.
    pub(super) fn expire(&mut self, now_ms: u64, change: &mut Change) {
        while let Some((&seq, key)) = self.by_write.first_key_value() {
            change.inspected = change.inspected.saturating_add(1);
            if self.table.get(key).is_some_and(|slot| slot.expires_at_ms > now_ms) {
                break;
            }
            self.remove_seq(seq, change);
        }
    }

    /// Write `entry` under `key`, expiring at `expires_at_ms`; a key already
    /// held keeps one entry, the new one, at the back of the write order.
    /// Past a bound the oldest writes are forgotten, the new one included
    /// when it alone exceeds the byte bound.
    pub(super) fn insert(
        &mut self,
        key: &str,
        entry: CancelEntry,
        expires_at_ms: u64,
        change: &mut Change,
    ) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        let bytes = footprint(key, &entry);
        let shared = match self.table.get_key_value(key) {
            Some((shared, slot)) => {
                let (shared, old) = (shared.clone(), slot.seq);
                self.remove_seq(old, change);
                shared
            }
            None => Arc::from(key),
        };
        self.by_write.insert(seq, shared.clone());
        self.table.insert(shared, Slot { entry, seq, expires_at_ms, bytes });
        self.bytes = self.bytes.saturating_add(bytes);
        change.added = change.added.saturating_add(1);
        change.bytes_added = change.bytes_added.saturating_add(bytes);
        while self.table.len() > self.max_entries || self.bytes > self.max_bytes {
            let Some((&front, _)) = self.by_write.first_key_value() else { break };
            self.remove_seq(front, change);
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
        if slot.expires_at_ms > now_ms {
            return Some(slot.entry.clone());
        }
        let seq = slot.seq;
        self.remove_seq(seq, change);
        None
    }

    /// Forget the entry under `key`, if held.
    pub(super) fn remove(&mut self, key: &str, change: &mut Change) {
        if let Some(seq) = self.table.get(key).map(|slot| slot.seq) {
            self.remove_seq(seq, change);
        }
    }

    fn remove_seq(&mut self, seq: u64, change: &mut Change) {
        let Some(key) = self.by_write.remove(&seq) else { return };
        let Some(slot) = self.table.remove(&key) else { return };
        self.bytes = self.bytes.saturating_sub(slot.bytes);
        change.removed = change.removed.saturating_add(1);
        change.bytes_removed = change.bytes_removed.saturating_add(slot.bytes);
    }
}
