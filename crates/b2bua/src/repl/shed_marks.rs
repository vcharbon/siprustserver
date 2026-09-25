//! [`ShedMarks`]: the backup replicas a node left unstored at a backup ceiling
//! (ADR-0037), each with the last changelog position its puller could claim
//! before the shed write. While a mark stands, the flow's reported position
//! stays at or below it, so the primary never reads its backup as holding a
//! call it does not (ADR-0031 D2).

use std::collections::{BTreeSet, HashMap};

use repl_net::frame::Watermark;

struct Mark {
    primary: String,
    /// The incarnation of the primary that sent the shed write.
    gen: u64,
    floor: Watermark,
    expiry_at_ms: Option<i64>,
}

/// Shed replicas by callRef, and their floors by primary.
#[derive(Default)]
pub(super) struct ShedMarks {
    by_ref: HashMap<String, Mark>,
    floors: HashMap<String, BTreeSet<(Watermark, String)>>,
}

impl ShedMarks {
    /// Mark `call_ref` of `primary` shed with `floor`, the shed write sent by
    /// incarnation `gen`. A ref already marked keeps its first floor: nothing
    /// past it has been held since.
    pub(super) fn mark(
        &mut self,
        call_ref: &str,
        primary: &str,
        gen: u64,
        floor: Watermark,
        expiry_at_ms: Option<i64>,
    ) {
        if let Some(m) = self.by_ref.get_mut(call_ref) {
            m.expiry_at_ms = expiry_at_ms;
            return;
        }
        self.floors.entry(primary.to_string()).or_default().insert((floor, call_ref.to_string()));
        let mark = Mark { primary: primary.to_string(), gen, floor, expiry_at_ms };
        self.by_ref.insert(call_ref.to_string(), mark);
    }

    /// Drop `primary`'s marks sent by an incarnation older than `gen`.
    pub(super) fn drop_older(&mut self, primary: &str, gen: u64) {
        let Some(set) = self.floors.get(primary) else { return };
        let stale: Vec<String> = set
            .iter()
            .filter(|(_, r)| self.by_ref.get(r).is_some_and(|m| m.gen < gen))
            .map(|(_, r)| r.clone())
            .collect();
        for call_ref in stale {
            self.clear(&call_ref);
        }
    }

    /// Drop the mark on `call_ref`: its replica is stored, or its call gone.
    pub(super) fn clear(&mut self, call_ref: &str) {
        let Some(m) = self.by_ref.remove(call_ref) else { return };
        if let Some(set) = self.floors.get_mut(&m.primary) {
            set.remove(&(m.floor, call_ref.to_string()));
            if set.is_empty() {
                self.floors.remove(&m.primary);
            }
        }
    }

    /// Drop every mark of `primary`.
    pub(super) fn clear_primary(&mut self, primary: &str) {
        if let Some(set) = self.floors.remove(primary) {
            for (_, call_ref) in set {
                self.by_ref.remove(&call_ref);
            }
        }
    }

    /// The lowest floor among `primary`'s marks.
    pub(super) fn floor(&self, primary: &str) -> Option<Watermark> {
        self.floors.get(primary).and_then(|set| set.first()).map(|(w, _)| *w)
    }

    /// Drop every mark expired at `now_ms`: its call outlived the backstop a
    /// stored replica would have had.
    pub(super) fn reap(&mut self, now_ms: i64) {
        let expired: Vec<String> = self
            .by_ref
            .iter()
            .filter(|(_, m)| matches!(m.expiry_at_ms, Some(e) if now_ms >= e))
            .map(|(k, _)| k.clone())
            .collect();
        for call_ref in expired {
            self.clear(&call_ref);
        }
    }

    /// Marks standing.
    pub(super) fn len(&self) -> usize {
        self.by_ref.len()
    }
}

#[cfg(test)]
mod shed_marks_tests {
    use super::*;

    fn w(counter: u64) -> Watermark {
        Watermark::new(1, counter)
    }

    #[test]
    fn the_floor_is_the_lowest_standing_mark_of_the_primary() {
        let mut marks = ShedMarks::default();
        assert_eq!(marks.floor("w0"), None);
        marks.mark("w0|a|t", "w0", 1, w(5), None);
        marks.mark("w0|b|t", "w0", 1, w(9), None);
        marks.mark("w2|c|t", "w2", 1, w(1), None);
        assert_eq!(marks.floor("w0"), Some(w(5)));
        marks.clear("w0|a|t");
        assert_eq!(marks.floor("w0"), Some(w(9)));
        marks.clear("w0|b|t");
        assert_eq!(marks.floor("w0"), None);
        assert_eq!(marks.floor("w2"), Some(w(1)), "primaries are independent");
    }

    #[test]
    fn a_ref_shed_again_keeps_its_first_floor() {
        let mut marks = ShedMarks::default();
        marks.mark("w0|a|t", "w0", 1, w(5), None);
        marks.mark("w0|a|t", "w0", 1, w(8), None);
        assert_eq!(marks.floor("w0"), Some(w(5)));
        marks.clear("w0|a|t");
        assert_eq!(marks.len(), 0);
        assert_eq!(marks.floor("w0"), None);
    }

    #[test]
    fn a_primary_is_cleared_alone() {
        let mut marks = ShedMarks::default();
        marks.mark("w0|a|t", "w0", 1, w(5), None);
        marks.mark("w2|c|t", "w2", 1, w(1), None);
        marks.clear_primary("w0");
        assert_eq!(marks.floor("w0"), None);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks.floor("w2"), Some(w(1)));
    }

    #[test]
    fn marks_of_an_older_incarnation_are_dropped() {
        let mut marks = ShedMarks::default();
        marks.mark("w0|old|t", "w0", 1, Watermark::new(1, 4), None);
        marks.mark("w0|scan|t", "w0", 2, Watermark::new(0, 0), None);
        marks.drop_older("w0", 2);
        assert_eq!(marks.len(), 1, "a mark made while scanning the new incarnation stays");
        assert_eq!(marks.floor("w0"), Some(Watermark::new(0, 0)));
        marks.drop_older("w0", 3);
        assert_eq!(marks.len(), 0);
    }

    #[test]
    fn an_expired_mark_is_reaped() {
        let mut marks = ShedMarks::default();
        marks.mark("w0|a|t", "w0", 1, w(5), Some(1_000));
        marks.mark("w0|b|t", "w0", 1, w(7), None);
        marks.reap(999);
        assert_eq!(marks.len(), 2);
        marks.reap(1_000);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks.floor("w0"), Some(w(7)));
    }
}
