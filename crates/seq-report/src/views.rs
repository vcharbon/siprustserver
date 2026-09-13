//! The **views plane** — who believed what about whom, and where they disagreed.
//!
//! The message planes say what crossed the wire; this one says what each actor
//! *held true* while it did. A cluster's hardest failures are disagreements: the
//! front proxy believes a worker is gone while the worker's process is still
//! bound and serving, a survivor believes a peer is parked while the orchestrator
//! has already admitted its replacement. A timeline of messages alone never shows
//! that — both sides are behaving correctly for the world they believe in.
//!
//! A [`ViewChange`] is one observer's belief about one subject, recorded at the
//! instant it changed. Observers describe the same world in their own words (a
//! registry says `present/Alive`, a process says `running gen 1`), so a belief
//! carries BOTH its prose and a [`ViewChange::stance`] — the short comparable
//! classification (`present` / `absent` / `draining` / `dead`) the projector
//! assigns. Prose is displayed; stance is what agreement is judged on. From the
//! change stream this module derives the two views a reader needs, as pure
//! functions over neutral strings (the renderer stays a leaf crate — the
//! projector owns the vocabulary):
//!
//! - [`views_table`] — one row per (instant, subject) at which some belief moved,
//!   one column per observer, each cell that observer's belief about that subject
//!   at that instant. A row where two observers hold different beliefs is
//!   `disputed`.
//! - [`disagreements`] — the intervals during which a subject's observers did not
//!   agree, each naming the differing beliefs and the signal each came from.

/// One observer's belief about one subject, recorded at the instant it changed.
/// `signal` names what produced the belief (the primitive that acted, or the
/// component that was read).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewChange {
    /// Virtual-clock timestamp (ms) — the display label, shared with [`crate::SeqRow`].
    pub at_ms: i64,
    /// Global recording-order sequence — the render-order key.
    pub seq: u64,
    /// Who holds the belief (e.g. `orchestrator`, `proxy`, `b1#g2`) — the
    /// table column and the chip's name.
    pub observer: String,
    /// The diagram column the chip anchors on (a [`crate::Lane::id`]). The
    /// projector resolves it; an unknown id anchors on the first lane.
    pub lane: String,
    /// What the belief is about (e.g. the worker ordinal `b1`).
    pub subject: String,
    /// The belief in the observer's own words (e.g. `present/Alive`, `parked`,
    /// `running gen 2`) — displayed, never compared.
    pub belief: String,
    /// The comparable classification of `belief` (e.g. `present`, `absent`,
    /// `draining`, `dead`). Two observers AGREE about a subject exactly when
    /// their stances match, so differing vocabularies do not read as conflict.
    pub stance: String,
    /// What produced it (e.g. `withdraw`, `proxy registry`, `supervisor peers`).
    pub signal: String,
}

/// One observer's belief as a table cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewCell {
    /// The belief held at this instant, in the observer's own words.
    pub belief: String,
    /// Its comparable classification (see [`ViewChange::stance`]).
    pub stance: String,
    /// The signal the belief came from.
    pub signal: String,
    /// Whether THIS instant is where the observer moved to it.
    pub changed: bool,
}

/// One row of the views table: an instant at which at least one observer's
/// belief about `subject` moved, with every observer's belief about that subject
/// as it stood after the move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewsRow {
    /// The instant (ms).
    pub at_ms: i64,
    /// The lowest recording sequence of the changes at this instant.
    pub seq: u64,
    /// Who the row is about.
    pub subject: String,
    /// One entry per observer (in the table's `observers` order); `None` when
    /// that observer has stated no belief about the subject yet.
    pub cells: Vec<Option<ViewCell>>,
    /// Two or more observers hold DIFFERENT stances about the subject here.
    pub disputed: bool,
}

/// The views table: the observer columns plus one row per change instant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewsTable {
    /// Column order: `orchestrator` first, then every other observer by name.
    pub observers: Vec<String>,
    /// Rows in timeline order.
    pub rows: Vec<ViewsRow>,
}

/// One interval during which the observers of a subject did not agree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disagreement {
    /// When the disagreement started (ms).
    pub from_ms: i64,
    /// When it ended (ms), or `None` if it was still open at the end of the run.
    pub to_ms: Option<i64>,
    /// Who the disagreement is about.
    pub subject: String,
    /// Who held what: `(observer, belief, signal)`, observer-ordered — every
    /// observer with a stated belief, so the conflicting sides read together.
    pub holders: Vec<(String, String, String)>,
}

/// The observer column order: `orchestrator` first (it acts on the cluster, so
/// it reads as the left-hand truth), then every other observer by name.
fn observer_order(views: &[ViewChange]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for v in views {
        if !names.contains(&v.observer) {
            names.push(v.observer.clone());
        }
    }
    names.sort_by(|a, b| {
        let rank = |s: &str| u8::from(s != "orchestrator");
        rank(a).cmp(&rank(b)).then(a.cmp(b))
    });
    names
}

/// The changes in render order (by `seq`, `at_ms` only as a tiebreaker), grouped
/// into instants: every change sharing one `at_ms` is one instant, so a
/// primitive that moves three observers at once produces ONE table row.
fn instants(views: &[ViewChange]) -> Vec<Vec<&ViewChange>> {
    let mut ordered: Vec<&ViewChange> = views.iter().collect();
    ordered.sort_by(|a, b| a.seq.cmp(&b.seq).then(a.at_ms.cmp(&b.at_ms)));
    let mut out: Vec<Vec<&ViewChange>> = Vec::new();
    for v in ordered {
        match out.last_mut() {
            Some(group) if group[0].at_ms == v.at_ms => group.push(v),
            _ => out.push(vec![v]),
        }
    }
    out
}

/// One observer's current belief about one subject, as the walk carries it
/// forward between instants.
#[derive(Clone)]
struct Held {
    observer: String,
    subject: String,
    belief: String,
    stance: String,
    signal: String,
}

/// Apply every change of one instant to the carried state and return the
/// subjects it touched, in first-seen order.
fn apply(group: &[&ViewChange], held: &mut Vec<Held>) -> Vec<String> {
    let mut touched: Vec<String> = Vec::new();
    for v in group {
        let entry = Held {
            observer: v.observer.clone(),
            subject: v.subject.clone(),
            belief: v.belief.clone(),
            stance: v.stance.clone(),
            signal: v.signal.clone(),
        };
        match held.iter_mut().find(|h| h.observer == v.observer && h.subject == v.subject) {
            Some(slot) => *slot = entry,
            None => held.push(entry),
        }
        if !touched.contains(&v.subject) {
            touched.push(v.subject.clone());
        }
    }
    touched
}

/// What every observer (in column order) believes about `subject` right now.
fn stated<'a>(held: &'a [Held], observers: &[String], subject: &str) -> Vec<&'a Held> {
    observers
        .iter()
        .filter_map(|obs| held.iter().find(|h| h.observer == *obs && h.subject == subject))
        .collect()
}

/// Build the views table from the recorded changes (see the module docs).
pub fn views_table(views: &[ViewChange]) -> ViewsTable {
    let observers = observer_order(views);
    let mut held: Vec<Held> = Vec::new();
    let mut rows: Vec<ViewsRow> = Vec::new();

    for group in instants(views) {
        for subject in apply(&group, &mut held) {
            let cells: Vec<Option<ViewCell>> = observers
                .iter()
                .map(|obs| {
                    held.iter().find(|h| h.observer == *obs && h.subject == subject).map(|h| {
                        ViewCell {
                            belief: h.belief.clone(),
                            stance: h.stance.clone(),
                            signal: h.signal.clone(),
                            changed: group
                                .iter()
                                .any(|v| v.observer == *obs && v.subject == subject),
                        }
                    })
                })
                .collect();
            let disputed = distinct_beliefs(&cells) > 1;
            rows.push(ViewsRow {
                at_ms: group[0].at_ms,
                seq: group.iter().map(|v| v.seq).min().unwrap_or(0),
                subject,
                cells,
                disputed,
            });
        }
    }
    ViewsTable { observers, rows }
}

/// How many DIFFERENT stances the stated cells hold.
fn distinct_beliefs(cells: &[Option<ViewCell>]) -> usize {
    let mut seen: Vec<&str> = Vec::new();
    for c in cells.iter().flatten() {
        if !seen.contains(&c.stance.as_str()) {
            seen.push(&c.stance);
        }
    }
    seen.len()
}

/// The intervals during which a subject's observers disagreed — held DIFFERENT
/// stances. An interval runs while the exact set of `(observer, belief)` pairs
/// is unchanged and the stances are not unanimous; the next change to that
/// subject closes it, and opens the next one if the observers still disagree.
/// `to_ms` is `None` for a disagreement still open at the end of the run.
pub fn disagreements(views: &[ViewChange]) -> Vec<Disagreement> {
    let observers = observer_order(views);
    let mut held: Vec<Held> = Vec::new();
    let mut open: Vec<Disagreement> = Vec::new();
    let mut out: Vec<Disagreement> = Vec::new();

    for group in instants(views) {
        for subject in apply(&group, &mut held) {
            let stated = stated(&held, &observers, &subject);
            let holders: Vec<(String, String, String)> = stated
                .iter()
                .map(|h| (h.observer.clone(), h.belief.clone(), h.signal.clone()))
                .collect();
            let mut distinct: Vec<&str> = Vec::new();
            for h in &stated {
                if !distinct.contains(&h.stance.as_str()) {
                    distinct.push(&h.stance);
                }
            }
            // Close whatever was open for this subject — its state just moved.
            if let Some(pos) = open.iter().position(|d| d.subject == subject) {
                let mut d = open.remove(pos);
                if d.holders != holders {
                    d.to_ms = Some(group[0].at_ms);
                    out.push(d);
                } else {
                    open.push(d); // unchanged holders: the interval continues
                }
            }
            if distinct.len() > 1 && !open.iter().any(|d| d.subject == subject) {
                open.push(Disagreement {
                    from_ms: group[0].at_ms,
                    to_ms: None,
                    subject: subject.clone(),
                    holders,
                });
            }
        }
    }
    out.extend(open);
    out.sort_by(|a, b| a.from_ms.cmp(&b.from_ms).then(a.subject.cmp(&b.subject)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chg(
        at_ms: i64,
        seq: u64,
        observer: &str,
        belief: &str,
        stance: &str,
        signal: &str,
    ) -> ViewChange {
        ViewChange {
            at_ms,
            seq,
            observer: observer.into(),
            lane: observer.into(),
            subject: "b1".into(),
            belief: belief.into(),
            stance: stance.into(),
            signal: signal.into(),
        }
    }

    /// The withdrawn-but-running shape: the orchestrator withdraws the endpoint,
    /// the proxy drops the ordinal, the process still says it is running.
    fn withdrawn() -> Vec<ViewChange> {
        vec![
            chg(0, 1, "b1#g1", "running gen 1", "present", "core"),
            chg(0, 2, "proxy", "present/Alive", "present", "proxy registry"),
            chg(100, 3, "orchestrator", "withdrawn", "absent", "withdraw"),
            chg(100, 4, "proxy", "absent", "absent", "withdraw"),
        ]
    }

    #[test]
    fn changes_sharing_an_instant_collapse_into_one_row() {
        let t = views_table(&withdrawn());
        assert_eq!(t.rows.len(), 2, "two instants: t=0 and t=100");
        assert_eq!(t.observers, vec!["orchestrator", "b1#g1", "proxy"]);
        let last = t.rows.last().unwrap();
        assert_eq!(last.at_ms, 100);
        // orchestrator + proxy moved together; the worker's belief carries forward.
        assert!(last.cells[0].as_ref().unwrap().changed);
        assert_eq!(last.cells[1].as_ref().unwrap().belief, "running gen 1");
        assert!(!last.cells[1].as_ref().unwrap().changed);
        assert_eq!(last.cells[2].as_ref().unwrap().belief, "absent");
    }

    #[test]
    fn a_row_whose_observers_differ_is_disputed() {
        let t = views_table(&withdrawn());
        assert!(!t.rows[0].disputed, "different words, one stance (present) — not a disagreement",);
        assert!(t.rows[1].disputed, "the proxy says absent while the process runs");
    }

    #[test]
    fn disagreement_names_the_interval_beliefs_and_signals() {
        let mut views = withdrawn();
        // The process is killed at t=300: everyone agrees it is dead.
        views.push(chg(300, 5, "b1#g1", "dead gen 1", "dead", "crash"));
        views.push(chg(300, 6, "orchestrator", "killed gen 1", "dead", "crash"));
        views.push(chg(300, 7, "proxy", "present/Dead", "dead", "proxy registry"));
        let d = disagreements(&views);
        let last = d.last().expect("a disagreement was recorded");
        assert_eq!(last.subject, "b1");
        assert_eq!(last.from_ms, 100);
        assert_eq!(last.to_ms, Some(300), "closed when the beliefs converged");
        assert!(last
            .holders
            .iter()
            .any(|(o, b, s)| o == "proxy" && b == "absent" && s == "withdraw"));
        assert!(last.holders.iter().any(|(o, b, _)| o == "b1#g1" && b == "running gen 1"));
    }

    #[test]
    fn unanimous_observers_produce_no_disagreement() {
        let views = vec![
            chg(0, 1, "proxy", "present/Alive", "present", "proxy registry"),
            chg(0, 2, "b2#g1", "active", "present", "supervisor peers"),
        ];
        assert!(disagreements(&views).is_empty());
        assert!(!views_table(&views).rows[0].disputed);
    }

    #[test]
    fn a_disagreement_open_at_the_end_has_no_close() {
        let d = disagreements(&withdrawn());
        assert_eq!(d.last().unwrap().to_ms, None);
    }
}
