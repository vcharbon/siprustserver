//! The two sides of a replica refused at a backup's memory ceiling (ADR-0037
//! item 4, ADR-0055 §3): on the backup, the calls whose replica it refused and
//! refuses again whatever room it finds; on the primary, the live calls that
//! have no backup for their whole life.

use std::collections::{HashMap, HashSet};

/// The replicas a backup refused, by callRef: each primary's write of one is
/// refused again until the call's delete, the mark's expiry or a new scan of
/// the primary.
#[derive(Default)]
pub(super) struct RefusedReplicas {
    /// callRef → (its primary, the mark's expiry).
    by_ref: HashMap<String, (String, Option<i64>)>,
}

impl RefusedReplicas {
    /// Refuse `call_ref` of `primary`, the mark expiring at `expiry_at_ms`.
    pub(super) fn refuse(&mut self, call_ref: &str, primary: &str, expiry_at_ms: Option<i64>) {
        self.by_ref.insert(call_ref.to_string(), (primary.to_string(), expiry_at_ms));
    }

    pub(super) fn contains(&self, call_ref: &str) -> bool {
        self.by_ref.contains_key(call_ref)
    }

    /// The call is gone.
    pub(super) fn clear(&mut self, call_ref: &str) {
        self.by_ref.remove(call_ref);
    }

    /// Every mark of `primary`: a new scan of it states what it backs up.
    pub(super) fn clear_primary(&mut self, primary: &str) {
        self.by_ref.retain(|_, (p, _)| p != primary);
    }

    /// Drop every mark expired at `now_ms`: its call outlived the backstop a
    /// stored replica would have had.
    pub(super) fn reap(&mut self, now_ms: i64) {
        self.by_ref.retain(|_, (_, expiry)| !matches!(expiry, Some(e) if now_ms >= *e));
    }

    pub(super) fn len(&self) -> usize {
        self.by_ref.len()
    }
}

/// The live calls of a primary that have no backup: admitted over a backlog
/// high-water mark, or refused by their backup. Each leaves with its call.
#[derive(Default)]
pub(super) struct Unbacked(HashSet<String>);

impl Unbacked {
    pub(super) fn insert(&mut self, call_ref: &str) {
        self.0.insert(call_ref.to_string());
    }

    pub(super) fn contains(&self, call_ref: &str) -> bool {
        self.0.contains(call_ref)
    }

    pub(super) fn remove(&mut self, call_ref: &str) {
        self.0.remove(call_ref);
    }

    pub(super) fn len(&self) -> usize {
        self.0.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_stands_until_cleared_or_expired() {
        let mut refused = RefusedReplicas::default();
        refused.refuse("w0|a|t", "w0", Some(1_000));
        refused.refuse("w0|b|t", "w0", None);
        refused.refuse("w2|c|t", "w2", None);
        assert!(refused.contains("w0|a|t"));
        refused.reap(999);
        assert_eq!(refused.len(), 3);
        refused.reap(1_000);
        assert!(!refused.contains("w0|a|t"), "expired with its backstop");
        refused.clear_primary("w0");
        assert!(!refused.contains("w0|b|t"));
        assert!(refused.contains("w2|c|t"), "primaries are independent");
        refused.clear("w2|c|t");
        assert_eq!(refused.len(), 0);
    }
}
