//! [`ResurrectionTombstones`]: the apply-side resurrection guard (ADR-0014
//! §"Terminal reconcile", rule 3). A deleted call is buried for
//! [`RESURRECTION_TOMBSTONE_MS`]: a `Put` of that call inside the window is
//! ignored, so a late reverse-flush racing a discharge never re-creates it. The
//! tombstone names the call incarnation it buries, not the `call_ref`: a new
//! call born on the same ref (a retried INVITE reusing its Call-ID and From
//! tag) is stored and replicated at once, and every call the ref carried stays
//! buried. A call a write of another one replaced is buried the same way.

use std::collections::HashMap;

/// How long a deleted call rejects re-creating `Put`s. A discharge deletes the
/// call and propagates the delete, but a peer's late reverse-flush (a backup
/// finishing its deferred teardown just after the primary discharged the
/// reclaimed copy) would otherwise re-create the body via the Reverse "no local
/// copy → accept" rule and trigger a SECOND discharge. The window need only
/// outlive replication latency + the served call's residual timers (Timer F
/// ~32 s) + a reboot; 5 min is comfortably past that and bounds the set to
/// `delete_rate × 5min`.
pub(super) const RESURRECTION_TOMBSTONE_MS: i64 = 300_000;

struct Tombstone {
    buried_at_ms: i64,
    /// The incarnation buried; `None` when the deleting node held no body of
    /// the ref and the delete named none.
    incarnation: Option<String>,
    burial: Burial,
}

/// How a call came to be buried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Burial {
    /// A delete removed it: whoever deleted it settled it.
    Deleted,
    /// A write of another call on the ref replaced it, the authority having
    /// moved on with no delete this node saw. Nobody has settled it; a backup
    /// hands what it still writes of it to the replica reap.
    Replaced,
}

/// Buried calls by `call_ref` — every call a ref carried inside the window,
/// each with its own deadline — pruned past their window by
/// [`prune`](Self::prune).
#[derive(Default)]
pub(super) struct ResurrectionTombstones {
    by_ref: HashMap<String, Vec<Tombstone>>,
}

impl ResurrectionTombstones {
    /// Bury `incarnation` of `call_ref` at `at_ms`, as `burial`. A call buried
    /// again is re-stamped; the ref's other calls stay buried.
    pub(super) fn bury(
        &mut self,
        call_ref: &str,
        incarnation: Option<String>,
        burial: Burial,
        at_ms: i64,
    ) {
        let buried = self.by_ref.entry(call_ref.to_string()).or_default();
        buried.retain(|t| t.incarnation != incarnation);
        buried.push(Tombstone { buried_at_ms: at_ms, incarnation, burial });
    }

    /// How a tombstone inside its window at `now_ms` buries a `Put` of
    /// `incarnation` of `call_ref`, `None` when none does. A `Put` naming no
    /// incarnation is taken as one of any buried call; a tombstone naming none
    /// buries only such a `Put`, since the deleting node held no body, so no
    /// named `Put` can bring one of its calls back. A deleted call answers
    /// [`Deleted`](Burial::Deleted) before a replaced one.
    pub(super) fn burial(
        &self,
        call_ref: &str,
        incarnation: Option<&str>,
        now_ms: i64,
    ) -> Option<Burial> {
        let buried = self.by_ref.get(call_ref)?;
        buried
            .iter()
            .filter(|t| now_ms - t.buried_at_ms < RESURRECTION_TOMBSTONE_MS)
            .filter(|t| incarnation.is_none() || t.incarnation.as_deref() == incarnation)
            .map(|t| t.burial)
            .min_by_key(|b| *b != Burial::Deleted)
    }

    /// Drop every tombstone past its window at `now_ms`. Nothing reads one
    /// older, so the set holds at most `delete_rate × RESURRECTION_TOMBSTONE_MS`
    /// entries.
    pub(super) fn prune(&mut self, now_ms: i64) {
        self.by_ref.retain(|_, buried| {
            buried.retain(|t| now_ms - t.buried_at_ms < RESURRECTION_TOMBSTONE_MS);
            !buried.is_empty()
        });
    }

    /// Refs with a standing tombstone, pruned or not.
    pub(super) fn len(&self) -> usize {
        self.by_ref.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DELETED: Burial = Burial::Deleted;

    #[test]
    fn a_tombstone_buries_the_call_it_names_and_no_other() {
        let mut t = ResurrectionTombstones::default();
        t.bury("r", Some("r#1".into()), DELETED, 0);
        assert_eq!(t.burial("r", Some("r#1"), 1_000), Some(DELETED), "the buried call");
        assert_eq!(t.burial("r", None, 1_000), Some(DELETED), "an unnamed write");
        assert_eq!(t.burial("r", Some("r#2"), 1_000), None, "another call on the ref");
        assert_eq!(t.burial("other", Some("r#1"), 1_000), None, "another ref");
        assert_eq!(t.burial("r", Some("r#1"), RESURRECTION_TOMBSTONE_MS), None, "past the window");
    }

    #[test]
    fn a_tombstone_naming_no_call_buries_only_unnamed_writes() {
        let mut t = ResurrectionTombstones::default();
        t.bury("r", None, DELETED, 0);
        assert_eq!(t.burial("r", None, 1_000), Some(DELETED));
        assert_eq!(t.burial("r", Some("r#1"), 1_000), None);
    }

    /// Every call a ref carried stays buried, each for its own window: the
    /// delete of a retry does not un-bury the call it retried.
    #[test]
    fn each_call_on_a_ref_stays_buried_for_its_own_window() {
        let mut t = ResurrectionTombstones::default();
        t.bury("r", Some("r#1".into()), DELETED, 0);
        t.bury("r", Some("r#2".into()), Burial::Replaced, 100_000);
        t.bury("r", None, DELETED, 100_000);
        assert_eq!(t.burial("r", Some("r#1"), 200_000), Some(DELETED));
        assert_eq!(t.burial("r", Some("r#2"), 200_000), Some(Burial::Replaced));
        assert_eq!(t.burial("r", Some("r#1"), RESURRECTION_TOMBSTONE_MS), None, "its own window");
        assert_eq!(t.burial("r", Some("r#2"), RESURRECTION_TOMBSTONE_MS), Some(Burial::Replaced));
    }

    #[test]
    fn prune_drops_tombstones_past_their_window() {
        let mut t = ResurrectionTombstones::default();
        t.bury("old", Some("old#1".into()), DELETED, 0);
        t.bury("new", Some("new#1".into()), DELETED, 100_000);
        t.bury("new", Some("new#0".into()), DELETED, 0);
        t.prune(RESURRECTION_TOMBSTONE_MS);
        assert_eq!(t.len(), 1);
        assert_eq!(t.burial("new", Some("new#1"), RESURRECTION_TOMBSTONE_MS), Some(DELETED));
        assert_eq!(t.burial("new", Some("new#0"), 0), None, "pruned within the ref too");
    }
}
