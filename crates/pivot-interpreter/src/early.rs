//! The **early dialogs** a leg's steps ride
//! (`PCAP2TEST_PIVOT_V3.md` §6.1 `early`, §8).
//!
//! A forking callee rings several times on one leg, and RFC 3261 §12.1.1 tells
//! them apart by the To-tag the UAS mints per early dialog. An `early` id is the
//! document's name for one of them, and which SIDE minted its tag gives the id
//! one of exactly two readings:
//!
//! - **Answered fork** — a response `send` on the leg carries the id. The leg's
//!   actor is the UAS, so the run MINTS one To-tag per `(leg, early)` before it
//!   speaks ([`EarlyDialogs`]), answers under it, and gates arrivals on it.
//! - **Observed fork** — a response `expect` on the leg carries the id. The
//!   PEER is the UAS, so the run LEARNS the tag from the first arrival consumed
//!   by a step naming the id ([`LearnedForks`]), and thereafter every step
//!   naming it requires that tag.
//!
//! An id is one or the other, never both, and every id must be one of the two —
//! [`PlanError`](crate::plan::PlanError) refuses the rest before a run exists.

use std::collections::BTreeMap;

use crate::plan::CompiledStep;

/// The ANSWERED forks of one run, keyed by leg and `early` id.
#[derive(Debug, Clone, Default)]
pub struct EarlyDialogs {
    tags: BTreeMap<(String, String), String>,
}

impl EarlyDialogs {
    /// Mint one To-tag per `(leg, early)` pair a response `send` answers under.
    /// An OBSERVED fork mints nothing — its tag is the peer's, learned in
    /// flight ([`LearnedForks`]).
    ///
    /// The run's nonce rides the tag for the same reason it rides the leg's own
    /// (RFC 3261 §8.1.1.4): two runs of one plan must not share a dialog
    /// identity. The id stays readable in front of it.
    pub fn mint<'s>(steps: impl IntoIterator<Item = &'s CompiledStep>, nonce: &str) -> Self {
        let mut tags = BTreeMap::new();
        for step in steps {
            let Some(early) = &step.early else { continue };
            if !(step.is_send() && step.msg.method.is_none()) {
                continue;
            }
            tags.entry((step.leg.clone(), early.clone()))
                .or_insert_with(|| format!("{}-{nonce}-early-{early}", step.leg));
        }
        EarlyDialogs { tags }
    }

    /// The To-tag `early` answers under on `leg`.
    pub fn tag(&self, leg: &str, early: &str) -> Option<&str> {
        self.tags.get(&(leg.to_string(), early.to_string())).map(String::as_str)
    }

    /// The To-tag a step rides, where it names an early dialog.
    pub fn tag_of(&self, step: &CompiledStep) -> Option<&str> {
        self.tag(&step.leg, step.early.as_deref()?)
    }

    /// Every fork this run minted, as `(leg, early id, tag)` — what the
    /// `${early:…}` namespace is published from.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str, &str)> {
        self.tags.iter().map(|((leg, early), tag)| (leg.as_str(), early.as_str(), tag.as_str()))
    }
}

/// The OBSERVED forks one run has learned, keyed by leg and `early` id.
///
/// Empty at the start of the run: an observed fork's To-tag is the peer's to
/// mint (RFC 3261 §12.1.1), so the runner binds `(leg, early)` to the tag the
/// first consumed arrival carried, and every later step naming the id requires
/// it. The binding is on CONSUMPTION, never on a speculative match — the gate
/// reads this map and writes nothing.
#[derive(Debug, Clone, Default)]
pub struct LearnedForks {
    tags: BTreeMap<(String, String), String>,
}

impl LearnedForks {
    /// The To-tag `early` is bound to on `leg`, once an arrival taught it.
    pub fn tag(&self, leg: &str, early: &str) -> Option<&str> {
        self.tags.get(&(leg.to_string(), early.to_string())).map(String::as_str)
    }

    /// The id on `leg` already bound to `tag`, if any — what keeps two observed
    /// forks from riding one dialog.
    pub fn holder(&self, leg: &str, tag: &str) -> Option<&str> {
        self.tags
            .iter()
            .find(|((known_leg, _), known_tag)| known_leg == leg && *known_tag == tag)
            .map(|((_, early), _)| early.as_str())
    }

    /// Bind `(leg, early)` to the tag the consumed arrival carried.
    pub fn bind(&mut self, leg: &str, early: &str, tag: &str) {
        self.tags.insert((leg.to_string(), early.to_string()), tag.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pivot_schema::flow::{Anchor, Delay};
    use pivot_schema::msg::MsgSpec;

    use crate::plan::{Discriminator, StepKind};
    use crate::program::StepLoc;

    fn step(id: &str, leg: &str, early: Option<&str>) -> CompiledStep {
        CompiledStep {
            id: id.into(),
            leg: leg.into(),
            kind: StepKind::Send,
            auto: false,
            after: Vec::new(),
            early: early.map(str::to_string),
            overlap: None,
            discriminator: Discriminator::Response { status: 183, cseq_method: None },
            msg: MsgSpec { status: Some(183), ..Default::default() },
            checks: Vec::new(),
            delay: Delay { ms: 0, from: Anchor::Trigger, compressible: false, timer_linked: false },
            within_ms: 1_000,
            retransmits: None,
            retransmit_intervals_ms: Vec::new(),
            loc: StepLoc { item: 0, branch: None, within: 0 },
            order: 0,
            deviations: Vec::new(),
        }
    }

    #[test]
    fn each_fork_of_a_leg_answers_under_its_own_tag() {
        let steps = [step("s4", "B", Some("f1")), step("s8", "B", Some("f2"))];
        let dialogs = EarlyDialogs::mint(&steps, "r1a2b3");
        let f1 = dialogs.tag("B", "f1").expect("f1 minted");
        let f2 = dialogs.tag("B", "f2").expect("f2 minted");
        assert_ne!(f1, f2, "two forks sharing a tag are one dialog");
        assert!(f1.starts_with("B-"), "{f1}");
    }

    /// One early dialog, however many steps ride it: the tag is the dialog's,
    /// not the step's.
    #[test]
    fn every_step_of_one_fork_reads_the_same_tag() {
        let steps = [step("s4", "B", Some("f1")), step("s7", "B", Some("f1"))];
        let dialogs = EarlyDialogs::mint(&steps, "r1a2b3");
        assert_eq!(dialogs.tag_of(&steps[0]), dialogs.tag_of(&steps[1]));
        assert_eq!(dialogs.tag_of(&step("s1", "B", None)), None);
    }

    /// The same id on two legs is two dialogs — a leg owns its own tag space.
    #[test]
    fn one_id_on_two_legs_is_two_dialogs() {
        let steps = [step("s4", "B", Some("f1")), step("s9", "C", Some("f1"))];
        let dialogs = EarlyDialogs::mint(&steps, "r1a2b3");
        assert_ne!(dialogs.tag("B", "f1"), dialogs.tag("C", "f1"));
    }

    /// Compile once, run MANY (§14): two runs of one plan mint disjoint forks.
    #[test]
    fn two_runs_of_one_plan_mint_disjoint_fork_tags() {
        let steps = [step("s4", "B", Some("f1"))];
        assert_ne!(
            EarlyDialogs::mint(&steps, "r1a2b3").tag("B", "f1"),
            EarlyDialogs::mint(&steps, "r9z8y7").tag("B", "f1")
        );
    }

    /// An OBSERVED fork's tag is the peer's to mint: a response expect naming an
    /// id mints nothing, so the run has nothing of its own to gate on until an
    /// arrival teaches it.
    #[test]
    fn an_observed_fork_mints_no_tag() {
        let mut observed = step("s5", "A", Some("r1"));
        observed.kind = StepKind::Expect { check: pivot_schema::flow::CheckMode::Record, optional: false };
        let dialogs = EarlyDialogs::mint(&[observed], "r1a2b3");
        assert_eq!(dialogs.tag("A", "r1"), None);
    }

    /// The learned map answers by id and by tag, and a binding is exactly what
    /// was bound: one id, one tag, one leg.
    #[test]
    fn a_learned_fork_is_bound_to_the_tag_that_taught_it() {
        let mut learned = LearnedForks::default();
        assert_eq!(learned.tag("A", "r1"), None);
        assert_eq!(learned.holder("A", "sut-tag-1"), None);
        learned.bind("A", "r1", "sut-tag-1");
        assert_eq!(learned.tag("A", "r1"), Some("sut-tag-1"));
        assert_eq!(learned.holder("A", "sut-tag-1"), Some("r1"));
        assert_eq!(learned.holder("A", "sut-tag-2"), None);
    }

    /// A tag bound on the leg is HELD: `holder` names the id riding it, which is
    /// what refuses a second id the same tag.
    #[test]
    fn a_tag_one_fork_rides_is_held_against_every_other_id() {
        let mut learned = LearnedForks::default();
        learned.bind("A", "r1", "sut-tag-1");
        assert_eq!(learned.holder("A", "sut-tag-1"), Some("r1"));
        assert_eq!(learned.tag("A", "r2"), None, "r2 stays unbound");
        learned.bind("A", "r2", "sut-tag-2");
        assert_eq!(learned.holder("A", "sut-tag-2"), Some("r2"));
        assert_eq!(learned.holder("A", "sut-tag-1"), Some("r1"), "r1's binding stands");
    }

    /// The same id on two legs is two bindings — a leg owns its own tag space,
    /// learned exactly as minted.
    #[test]
    fn one_learned_id_on_two_legs_is_two_bindings() {
        let mut learned = LearnedForks::default();
        learned.bind("A", "r1", "sut-tag-a");
        learned.bind("C", "r1", "sut-tag-c");
        assert_eq!(learned.tag("A", "r1"), Some("sut-tag-a"));
        assert_eq!(learned.tag("C", "r1"), Some("sut-tag-c"));
        assert_eq!(learned.holder("C", "sut-tag-a"), None, "A's tag holds nothing on C");
    }
}
