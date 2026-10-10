//! What a rule states about an occasion: the three-valued decision, its
//! evidence, and the population contract.

pub use crate::registry::RuleId;
use crate::wire::Endpoint;

/// What one occasion's wire proves. `Violated` carries the proof; `Compliant`
/// is an obligation observably met; `Undecidable` names why the observation
/// could not settle it. What to DO with each is consumer policy — the census
/// under-reports (no hit), a live gate may still refuse to pass — and the
/// population contract keeps that policy's cost visible.
#[derive(Debug, Clone)]
pub enum Decision {
    Violated(Evidence),
    Compliant,
    Undecidable(&'static str),
}

/// One occasion of one rule: WHO is charged, on which dialog and transaction,
/// and what the wire settled. `hits ⊆ decided ⊆ occasions` falls out of the
/// [`Decision`]: every finding is an occasion, non-`Undecidable` ones are
/// decided, `Violated` ones are hits.
#[derive(Debug, Clone)]
pub struct Finding {
    pub rule: RuleId,
    /// The endpoint charged: the one that emitted the offending message, or
    /// owed the one never emitted.
    pub emitter: Endpoint,
    /// The other side of the obligation: where the offending message went, or
    /// who was owed the one that never came.
    pub taker: Endpoint,
    /// CSeq NUMBER of the INVITE transaction the obligation rides.
    pub cseq: u32,
    /// The emitter forwarded the behaviour rather than originating it — a
    /// weaker finding, kept or dropped by consumer policy.
    pub relayed: bool,
    /// Index into the view's `msgs` of the message the occasion rests on, so
    /// reports read in observation order whichever rule decided them.
    pub anchor: usize,
    pub decision: Decision,
}

impl Finding {
    pub fn decided(&self) -> bool {
        !matches!(self.decision, Decision::Undecidable(_))
    }
    pub fn violated(&self) -> bool {
        matches!(self.decision, Decision::Violated(_))
    }
}

/// One rule's population over a set of findings: `hits ⊆ decided ⊆ occasions`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Population {
    pub occasions: u64,
    pub decided: u64,
    pub hits: u64,
}

impl Population {
    /// Fold one finding in.
    pub fn count(&mut self, f: &Finding) {
        self.occasions += 1;
        if f.decided() {
            self.decided += 1;
        }
        if f.violated() {
            self.hits += 1;
        }
    }

    /// Fold another population of the same rule in.
    pub fn add(&mut self, other: Population) {
        self.occasions += other.occasions;
        self.decided += other.decided;
        self.hits += other.hits;
    }
}

mod evidence;

pub use evidence::Evidence;
