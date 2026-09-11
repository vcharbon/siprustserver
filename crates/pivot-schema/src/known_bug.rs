//! Lane-declared KNOWN BUGS (`RunConfig.known_bugs`): divergences the system
//! under test on one lane is known to produce, named so a gate can stand down
//! for them without going quiet about it.
//!
//! A known bug is not an acceptance. It says the lane's SUT is wrong in a way
//! already written down, and that the run must go on rather than abandon the
//! leg on the first symptom — a divergence that stops the flow hides every
//! later step behind itself. What the run then observes is recorded and
//! classified downstream like any other difference.
//!
//! Closed vocabulary, for the reason `scoping`'s classes are closed: a token is
//! a promise that one named gate stands down for it, and a token nothing
//! implements is not a promise. It is a LANE fact — the document never states
//! one, and a lane that states none gates on everything.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One named SUT defect a lane declares.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum KnownBug {
    /// The SUT relays a provisional response without applying the 18x rewrite
    /// its routing decision armed, so a body the document expects stripped
    /// arrives on the 180. Waiving it costs the body assertion on provisional
    /// responses ONLY: every other declared body shape still gates, and the
    /// unstripped content stays visible in the confrontation.
    ProvisionalRewriteNotApplied,
}

impl fmt::Display for KnownBug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KnownBug::ProvisionalRewriteNotApplied => {
                f.write_str("provisional-rewrite-not-applied")
            }
        }
    }
}

impl FromStr for KnownBug {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "provisional-rewrite-not-applied" => Ok(KnownBug::ProvisionalRewriteNotApplied),
            other => Err(format!("known bug {other:?} is not in the closed vocabulary")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bug_round_trips_through_its_token() {
        for token in ["provisional-rewrite-not-applied"] {
            let bug: KnownBug = token.parse().unwrap();
            assert_eq!(bug.to_string(), token);
            assert_eq!(serde_json::to_string(&bug).unwrap(), format!("\"{token}\""));
        }
    }

    #[test]
    fn a_bug_outside_the_vocabulary_is_refused() {
        assert!("sut-is-slow".parse::<KnownBug>().is_err());
        assert!(serde_json::from_str::<KnownBug>("\"sut-is-slow\"").is_err());
    }
}
