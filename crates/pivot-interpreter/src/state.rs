//! **Runner state**: the dialog facts a run produces, and what each step saw.
//!
//! This is the only thing an accessor ever resolves against
//! (`PCAP2TEST_PIVOT_V3.md` §8.1). The document is immutable and shared across
//! call instances, so a leg accessor reading `${leg:B.remote-tag}` must read the
//! tag THIS instance minted — never a value the capture held.

use std::collections::BTreeMap;

use sip_message::HeaderProjection;

/// One leg's dialog state, as the run has learned it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LegState {
    pub call_id: Option<String>,
    pub local_tag: Option<String>,
    pub remote_tag: Option<String>,
    /// The peer's Contact, learned off the first dialog-establishing response
    /// or request.
    pub remote_target: Option<String>,
    /// The route set, in the order it must be applied.
    pub route_set: Vec<String>,
    /// The highest CSeq this side has SENT on the leg.
    pub cseq_local: Option<u32>,
    /// The highest CSeq the peer has sent on the leg.
    pub cseq_remote: Option<u32>,
    /// The last RSeq seen on a reliable provisional of this leg.
    pub rseq: Option<u32>,
}

/// One EARLY dialog's state, as the `${early:…}` namespace publishes it
/// (`PCAP2TEST_PIVOT_V3.md` §8.1).
///
/// A forking leg has one To-tag and one RSeq space PER FORK (RFC 3261 §12.1.1,
/// RFC 3262 §3), which is exactly what the single-valued leg fields cannot say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EarlyState {
    /// The leg whose UAS side answers under this id.
    pub leg: String,
    /// The To-tag this fork answers under, minted before the run speaks.
    pub tag: String,
    /// The RSeq of the last reliable provisional that rode this fork.
    pub rseq: Option<u32>,
    /// Whether a second leg declares the same id. A leg owns its own fork tag
    /// space, so the id then names two dialogs and resolves to neither.
    pub shared: bool,
}

/// What one step's message carried, kept for the `${step:…}` namespace and for
/// the post-run confrontation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepOutcome {
    pub status: Option<u16>,
    pub cseq: Option<u32>,
    pub cseq_method: Option<String>,
    pub rseq: Option<u32>,
    pub method: Option<String>,
    /// Headers in wire order, as the message carried them — the same projection
    /// [`Inbound`](crate::gate::Inbound) holds, so the two read identically.
    pub headers: HeaderProjection,
}

impl StepOutcome {
    /// The value of `name` by header IDENTITY — a compact spelling is found —
    /// taking the FIRST occurrence: wire order is what the recording preserves,
    /// so "the header" is the first one the message spelled.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.first(name)
    }
}

/// Everything one run instance has learned. Mutable for the run's duration and
/// discarded with it; the plan it runs is untouched.
#[derive(Debug, Clone, Default)]
pub struct RunState {
    legs: BTreeMap<String, LegState>,
    /// `early` id → the fork it names. Keyed by the id alone, as the accessor
    /// spells it.
    early: BTreeMap<String, EarlyState>,
    steps: BTreeMap<String, StepOutcome>,
    /// `alt` node id → the branch that committed.
    branches: BTreeMap<String, String>,
}

impl RunState {
    pub fn new() -> Self {
        RunState::default()
    }

    pub fn leg(&self, id: &str) -> Option<&LegState> {
        self.legs.get(id)
    }

    /// The leg's state, created empty on first use — a leg exists from the
    /// moment the run touches it.
    pub fn leg_mut(&mut self, id: &str) -> &mut LegState {
        self.legs.entry(id.to_string()).or_default()
    }

    pub fn early(&self, id: &str) -> Option<&EarlyState> {
        self.early.get(id)
    }

    /// Publish the tag one `early` id answers under, before the run speaks. A
    /// second leg claiming the id marks the entry SHARED rather than
    /// overwriting it: the accessor then names two dialogs, and picking one
    /// would be the inference §14 forbids.
    pub fn mint_early(&mut self, id: &str, leg: &str, tag: &str) {
        match self.early.get_mut(id) {
            Some(existing) if existing.leg != leg => existing.shared = true,
            Some(_) => {}
            None => {
                self.early.insert(
                    id.to_string(),
                    EarlyState {
                        leg: leg.to_string(),
                        tag: tag.to_string(),
                        rseq: None,
                        shared: false,
                    },
                );
            }
        }
    }

    /// The RSeq a reliable provisional put on one fork.
    pub fn record_early_rseq(&mut self, id: &str, rseq: u32) {
        if let Some(early) = self.early.get_mut(id) {
            early.rseq = Some(rseq);
        }
    }

    pub fn step(&self, id: &str) -> Option<&StepOutcome> {
        self.steps.get(id)
    }

    /// Record what a step's message carried. A step records once; a
    /// retransmission of the same step never overwrites the first sighting,
    /// because a later accessor must read what the step SAW, not what repeated.
    pub fn record_step(&mut self, id: &str, outcome: StepOutcome) {
        self.steps.entry(id.to_string()).or_insert(outcome);
    }

    #[cfg(test)]
    pub fn commit_branch(&mut self, alt: &str, branch: &str) {
        self.branches.insert(alt.to_string(), branch.to_string());
    }

    pub fn branch(&self, alt: &str) -> Option<&str> {
        self.branches.get(alt).map(String::as_str)
    }
}
