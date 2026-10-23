//! One admit of a call's key: what came back, and the report the call
//! applies (ADR-0040).

use serde::{Deserialize, Serialize};

use super::{LimiterEntry, LimiterHeld};

/// The honest outcome of one admit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmitOutcome {
    /// The call's set is now the entries sent, under the admit's number.
    Admitted,
    /// An id the call adds is at its cap: the call's set is what it was, or
    /// nothing when the admit asked for release on refusal, as `held` states.
    Rejected {
        /// The first id found at its cap.
        limiter_id: String,
        /// The set the limiter holds for the key after the refusal.
        held: LimiterHeld,
    },
    /// The admit's number is not above the one of the set the limiter holds
    /// for the key (a newer admit of the key landed first): nothing moved.
    Superseded {
        /// The set the limiter holds for the key.
        held: LimiterHeld,
    },
    /// The call was released within the last lease: the limiter holds nothing
    /// for it and re-creates nothing.
    Released,
    /// The request left and no usable answer came back (unreachable, slow,
    /// errored, a bad body): it may have landed.
    Unavailable,
    /// No request left (no limiter configured, or a local guard such as an
    /// open circuit breaker refused to send one): nothing can have landed.
    NotSent,
}

impl AdmitOutcome {
    /// The id a cap refusal names; `None` for any other outcome.
    pub fn refused_on(&self) -> Option<&str> {
        match self {
            AdmitOutcome::Rejected { limiter_id, .. } => Some(limiter_id),
            _ => None,
        }
    }

    /// The outcome's label on the wire of an internal event and in metrics.
    pub fn label(&self) -> &'static str {
        match self {
            AdmitOutcome::Admitted => "admitted",
            AdmitOutcome::Rejected { .. } => "rejected",
            AdmitOutcome::Superseded { .. } => "superseded",
            AdmitOutcome::Released => "released",
            AdmitOutcome::Unavailable => "unavailable",
            AdmitOutcome::NotSent => "not_sent",
        }
    }
}

/// One admit of a call's key as the call applies it: the key, the change
/// number the admit carried, the entries it asked for, and its outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmitReport {
    /// The limiter key the admit named.
    pub key: String,
    /// The change number the admit carried.
    pub change: u64,
    /// The entries the admit asked for.
    pub entries: Vec<LimiterEntry>,
    /// What came back.
    pub outcome: AdmitOutcome,
}

impl AdmitReport {
    /// The admit of `entries` for `key` under `change` whose answer never
    /// came: [`AdmitOutcome::Unavailable`].
    pub(super) fn lost(key: String, change: u64, entries: Vec<LimiterEntry>) -> Self {
        Self { key, change, entries, outcome: AdmitOutcome::Unavailable }
    }

    /// The key whose release this admit owes: a request left, and it may
    /// have landed. `None` for an admit that sent no request.
    pub fn owed_release(&self) -> Option<&str> {
        (self.outcome != AdmitOutcome::NotSent).then_some(self.key.as_str())
    }

    /// The set the limiter stated it holds for the key: the entries sent when
    /// admitted, the stated set on a refusal or a supersession, none
    /// otherwise.
    pub fn held(&self) -> Option<LimiterHeld> {
        match &self.outcome {
            AdmitOutcome::Admitted => {
                Some(LimiterHeld { change: self.change, entries: self.entries.clone() })
            }
            AdmitOutcome::Rejected { held, .. } | AdmitOutcome::Superseded { held } => {
                Some(held.clone())
            }
            AdmitOutcome::Released | AdmitOutcome::Unavailable | AdmitOutcome::NotSent => None,
        }
    }
}
