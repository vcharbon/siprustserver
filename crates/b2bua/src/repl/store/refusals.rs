//! The two sides of a replica refused at a backup's memory ceiling (ADR-0037
//! item 4, ADR-0055 §3): on the backup, the calls whose replica it refused and
//! refuses again whatever room it finds; on the primary, the live calls that
//! have no backup for their whole life.

use std::collections::{BTreeMap, HashMap, HashSet};

/// How long a refusal stands: it only has to outlive the writes of the call
/// already in flight when the primary reads the refusal, one transit and one
/// drain batch away.
pub(super) const REFUSAL_TTL_MS: i64 = 10_000;

/// The most refusals a backup keeps; past it the oldest goes first.
pub(super) const REFUSALS_MAX: usize = 65_536;

/// One refusal: the call incarnation refused, its primary, and when it lapses.
struct Refusal {
    incarnation: Option<String>,
    primary: String,
    expiry_at_ms: i64,
    /// Insertion order, the key of `order`.
    seq: u64,
}

/// The replicas a backup refused at its ceiling, by callRef: each write of the
/// refused call incarnation is refused again until the call's delete, the
/// refusal's TTL or a new scan of its primary. Bounded by
/// [`REFUSALS_MAX`], the oldest evicted first. A write of another call on the
/// ref (a retry on the identity) is not refused for the former call's mark.
#[derive(Default)]
pub(super) struct RefusedReplicas {
    by_ref: HashMap<String, Refusal>,
    /// `seq` → callRef, oldest first.
    order: BTreeMap<u64, String>,
    next_seq: u64,
}

impl RefusedReplicas {
    /// Refuse `incarnation` of `call_ref` of `primary` from `now_ms`.
    pub(super) fn refuse(
        &mut self,
        call_ref: &str,
        incarnation: Option<&str>,
        primary: &str,
        now_ms: i64,
    ) {
        self.clear(call_ref);
        while self.by_ref.len() >= REFUSALS_MAX {
            let Some((_, oldest)) = self.order.pop_first() else { break };
            self.by_ref.remove(&oldest);
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        let refusal = Refusal {
            incarnation: incarnation.map(str::to_string),
            primary: primary.to_string(),
            expiry_at_ms: now_ms.saturating_add(REFUSAL_TTL_MS),
            seq,
        };
        self.by_ref.insert(call_ref.to_string(), refusal);
        self.order.insert(seq, call_ref.to_string());
    }

    /// Whether a write of `incarnation` of `call_ref` is refused: the same call
    /// incarnation was, or a write naming none.
    pub(super) fn refuses(&self, call_ref: &str, incarnation: Option<&str>) -> bool {
        self.by_ref.get(call_ref).is_some_and(|r| {
            incarnation.is_none()
                || r.incarnation.is_none()
                || r.incarnation.as_deref() == incarnation
        })
    }

    /// The call is gone.
    pub(super) fn clear(&mut self, call_ref: &str) {
        if let Some(r) = self.by_ref.remove(call_ref) {
            self.order.remove(&r.seq);
        }
    }

    /// Every refusal of `primary`: a new scan of it states what it backs up.
    pub(super) fn clear_primary(&mut self, primary: &str) {
        let gone: Vec<String> = self
            .by_ref
            .iter()
            .filter(|(_, r)| r.primary == primary)
            .map(|(k, _)| k.clone())
            .collect();
        for call_ref in gone {
            self.clear(&call_ref);
        }
    }

    /// Drop every refusal lapsed at `now_ms`.
    pub(super) fn reap(&mut self, now_ms: i64) {
        let gone: Vec<String> = self
            .by_ref
            .iter()
            .filter(|(_, r)| now_ms >= r.expiry_at_ms)
            .map(|(k, _)| k.clone())
            .collect();
        for call_ref in gone {
            self.clear(&call_ref);
        }
    }

    #[cfg(test)]
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
    fn a_refusal_stands_until_cleared_or_lapsed() {
        let mut refused = RefusedReplicas::default();
        refused.refuse("w0|a|t", Some("i1"), "w0", 0);
        refused.refuse("w0|b|t", Some("i1"), "w0", 5_000);
        refused.refuse("w2|c|t", None, "w2", 5_000);
        assert!(refused.refuses("w0|a|t", Some("i1")));
        refused.reap(REFUSAL_TTL_MS - 1);
        assert_eq!(refused.len(), 3);
        refused.reap(REFUSAL_TTL_MS);
        assert!(!refused.refuses("w0|a|t", Some("i1")), "lapsed with its TTL");
        refused.clear_primary("w0");
        assert!(!refused.refuses("w0|b|t", Some("i1")));
        assert!(refused.refuses("w2|c|t", Some("any")), "primaries are independent");
        refused.clear("w2|c|t");
        assert_eq!(refused.len(), 0);
    }

    #[test]
    fn a_retry_on_the_ref_is_not_refused_for_the_former_calls_mark() {
        let mut refused = RefusedReplicas::default();
        refused.refuse("w0|a|t", Some("first"), "w0", 0);
        assert!(refused.refuses("w0|a|t", Some("first")));
        assert!(!refused.refuses("w0|a|t", Some("retry")), "another call on the ref");
    }

    #[test]
    fn the_oldest_refusal_goes_first_past_the_cap() {
        let mut refused = RefusedReplicas::default();
        for i in 0..=REFUSALS_MAX {
            refused.refuse(&format!("w0|{i}|t"), None, "w0", 0);
        }
        assert_eq!(refused.len(), REFUSALS_MAX);
        assert!(!refused.refuses("w0|0|t", None), "the oldest was evicted");
        assert!(refused.refuses(&format!("w0|{REFUSALS_MAX}|t"), None));
    }
}
