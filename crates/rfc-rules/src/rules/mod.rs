//! The rule bodies. One file per obligation family; a family's rules share
//! one wire walk and are consumed side by side. A walk more than one FAMILY
//! reads lives in its own module (`branch`) rather than in one family's
//! file — no rule file owns another's reading.

mod branch;

pub mod ack;
pub mod cancel;
pub mod capability;
pub mod correlation;
pub mod cseq;
pub mod dialog;
pub mod final_response;
pub mod offer_answer;
pub mod prack;
pub mod proxy;
pub mod register;
pub mod reinvite;
pub mod retransmit;
pub mod via;
pub mod wellformed;

use crate::registry::Rule;
use crate::verdict::Finding;
use crate::wire::WireView;

/// One RFC obligation, decided off the wire. `eval` returns EVERY occasion —
/// violated, compliant and undecidable alike — so the population contract
/// `hits ⊆ decided ⊆ occasions` is the caller's to fold, never to reconstruct.
/// Its id is the one its family's `rules!` row gives it.
pub trait Obligation: Send + Sync {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding>;
}

/// Every family's rule table: the registry reads the rules off these.
pub(crate) const FAMILIES: &[&[Rule]] = &[
    ack::RULES,
    cancel::RULES,
    capability::RULES,
    correlation::RULES,
    cseq::RULES,
    dialog::RULES,
    final_response::RULES,
    offer_answer::RULES,
    prack::RULES,
    proxy::RULES,
    register::RULES,
    reinvite::RULES,
    retransmit::RULES,
    via::RULES,
    wellformed::RULES,
];
