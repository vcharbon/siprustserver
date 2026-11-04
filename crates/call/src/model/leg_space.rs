//! The leg-id space a node acting on a call mints its legs from: the node
//! the call was created on mints `b-{n}`, any other node `c-{n}`, `n` one
//! more than the legs the call holds. Two copies of one call acting at once
//! never mint one id for two legs, so a leg id names one leg in any union of
//! their records.

use serde::{Deserialize, Serialize};

/// Which of a call's two leg-id spaces the node acting on it mints from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LegSpace {
    /// The space of the node the call was created on.
    #[default]
    Creator,
    /// The space of any other node.
    Other,
}

impl LegSpace {
    /// The prefix of the ids the space mints.
    pub fn prefix(self) -> &'static str {
        match self {
            LegSpace::Creator => "b",
            LegSpace::Other => "c",
        }
    }
}
