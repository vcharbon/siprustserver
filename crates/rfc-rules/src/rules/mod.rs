//! The rule bodies. One file per obligation family; a family's rules share
//! one wire walk and are consumed side by side. A walk more than one FAMILY
//! reads lives in its own module ([`branch`]) rather than in one family's
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

use crate::verdict::{Finding, RuleId};
use crate::wire::WireView;

/// One RFC obligation, decided off the wire. `eval` returns EVERY occasion —
/// violated, compliant and undecidable alike — so the population contract
/// `hits ⊆ decided ⊆ occasions` is the caller's to fold, never to reconstruct.
pub trait Obligation: Send + Sync {
    fn id(&self) -> RuleId;
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding>;
}
