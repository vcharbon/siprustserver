//! The `case` block (`PCAP2TEST_PIVOT_V3.md` §3): identity, provenance,
//! per-lane replayability, the capabilities a rig must have, and the
//! informative annotation sidecar.
//!
//! Lane names, verdict reasons and capability tokens are deployment vocabulary
//! — which lanes and capabilities exist is a property of a test rig, not of the
//! format — so `lanes` is an open map, a verdict's blocking reason is an open
//! token and `requires` is a list of open tokens. Only the `ok` / `blocked:`
//! grammar is fixed, because the driver's behavior turns on it.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Case identity, provenance and replayability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Case {
    /// Case directory name.
    pub id: String,
    /// One line, human-facing.
    pub title: String,
    /// Informative classification of the callflow shape. Open token; never
    /// interpreted by an interpreter or a lane.
    pub family: String,
    /// Which side of a fix the document describes.
    pub variant: Variant,
    /// Where the document came from. What the generator-subset lint gate turns
    /// on: a captured document may carry only what a capture can justify.
    pub origin: Origin,
    /// Where the case was cut from. Present exactly on a captured document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// The step whose outcome IS the defect, plus a description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defect: Option<Defect>,
    /// Capabilities the rig must have for this case to mean anything (`proxy`,
    /// `ha-pair`, `store-faults`, …). INFORMATIVE and open: the driver refuses
    /// an impossible scenario/scene pairing loudly instead of running a test
    /// that cannot fail. The interpreter never reads it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// The lane whose system produced the content this document asserts — the
    /// captured platform, or the deployment a test was authored against. Open
    /// token, and the one input to the lane-scoped downgrade of §9.1: replayed
    /// on another lane, a CLASSIFIED check is evaluated and recorded without
    /// gating. A document that states none downgrades nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_lane: Option<String>,
    /// Per-lane replayability verdict, keyed by lane name. The generator STATES
    /// whether a lane can replay the case; a driver skips a non-replayable lane
    /// loudly instead of failing it.
    pub lanes: BTreeMap<String, LaneVerdict>,
    /// Informative sidecar. The interpreter never reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Annotations>,
}

/// Which side of a fix a document describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Variant {
    /// Reproduces the defect: authored at issue time.
    Repro,
    /// The behavior the fix must produce: authored WITH the fix.
    Target,
}

/// How the document came to exist. The two are not styles of one thing: a
/// captured document is a projection of packets a tool saw, and everything it
/// carries must be readable off them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// Generated from a packet capture. Restricted to the generator subset
    /// (§13): no authored-only construct may appear.
    Capture,
    /// Written by hand or by an authoring program. The full format.
    Authored,
}

/// Where the case was cut from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// Capture file the case was cut from.
    pub capture: String,
    /// Upstream call groups the case spans.
    pub call_groups: Vec<u32>,
    /// Whether identities went through the anonymizer.
    pub anonymized: bool,
}

/// The defect a repro case exists to hold still.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Defect {
    /// Where in the flow the defect shows.
    pub marker: DefectMarker,
    /// What is wrong, in one human-facing sentence.
    pub description: String,
}

/// The flow step whose outcome IS the defect, named by its id — so inserting a
/// step ahead of it does not move the marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DefectMarker {
    /// Id of the step whose outcome IS the defect.
    pub step: String,
}

/// Generator diagnostics and detection evidence that drives nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Annotations {
    /// Decisions the generator owes a human, one per diagnostic.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<Flag>,
    /// Free prose for a reviewer: what the case proves, and what it does not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// One generator diagnostic: a decision owed, stated rather than guessed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Flag {
    /// Open token classifying the diagnostic, so a corpus is greppable by it.
    pub kind: String,
    /// What the generator saw, and what it could not decide.
    pub detail: String,
}

/// A lane's replayability verdict: `ok`, or `blocked:<reason>` with an open
/// reason token.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum LaneVerdict {
    /// The lane can replay this case.
    Ok,
    /// The lane cannot, for the named reason.
    Blocked(String),
}

impl LaneVerdict {
    /// Whether this lane may run the case.
    pub fn is_ok(&self) -> bool {
        matches!(self, LaneVerdict::Ok)
    }
}

impl fmt::Display for LaneVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LaneVerdict::Ok => f.write_str("ok"),
            LaneVerdict::Blocked(reason) => write!(f, "blocked:{reason}"),
        }
    }
}

impl FromStr for LaneVerdict {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ok" => Ok(LaneVerdict::Ok),
            other => match other.strip_prefix("blocked:") {
                Some("") => Err("lane verdict 'blocked:' states no reason".into()),
                Some(reason) => Ok(LaneVerdict::Blocked(reason.to_string())),
                None => {
                    Err(format!("lane verdict {other:?} is neither 'ok' nor 'blocked:<reason>'"))
                }
            },
        }
    }
}

crate::string_token!(LaneVerdict, "Lane verdict: `ok` or `blocked:<reason>`.", "^(ok|blocked:.+)$");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verdict_round_trips_through_its_token() {
        for token in ["ok", "blocked:number-unclassified", "blocked:claim-ambiguous"] {
            let v: LaneVerdict = token.parse().unwrap();
            assert_eq!(v.to_string(), token);
        }
        assert!("ok".parse::<LaneVerdict>().unwrap().is_ok());
        assert!(!"blocked:x".parse::<LaneVerdict>().unwrap().is_ok());
    }

    #[test]
    fn a_reasonless_or_unknown_verdict_is_refused() {
        assert!("blocked:".parse::<LaneVerdict>().is_err());
        assert!("skipped".parse::<LaneVerdict>().is_err());
    }
}
