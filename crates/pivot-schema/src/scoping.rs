//! Check classes (`PCAP2TEST_PIVOT_V3.md` §9.1): which assertions read the
//! ORIGIN platform's vocabulary rather than SIP's.
//!
//! A document carries facts about the system it was cut from. Some of those
//! facts are protocol — a status, a header SIP defines, a dialog identifier —
//! and hold on any system that speaks SIP. Others are one platform's spelling:
//! the headers it stamps on its own egress, the words its CDR writer uses. A
//! class NAMES which of the two an assertion reads, and it is stated by the
//! generator or the author, never inferred from a header name at replay time.
//!
//! What a class costs is a LANE decision, not a document one: replayed on the
//! lane it came from, a classified check gates like any other; replayed
//! elsewhere, it is evaluated, recorded and does not gate. `case.origin_lane`
//! is the lane the document's content came from, and an unclassified check
//! always gates.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What vocabulary an assertion reads. Closed: a class is a promise that one
/// named downgrade rule applies to it, and a rule nothing implements is not a
/// promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CheckClass {
    /// A header only the origin platform emits (`P-Charging-Vector`,
    /// `P-Identifier`, `P-Orig`). Another platform relays a compliant message
    /// that simply does not carry it.
    OriginPlatformHeader,
    /// The origin platform's CDR record vocabulary: event names, disposition
    /// words, field spellings. Another platform bills the same call in its own
    /// words.
    CdrVocabulary,
}

impl fmt::Display for CheckClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckClass::OriginPlatformHeader => f.write_str("origin-platform-header"),
            CheckClass::CdrVocabulary => f.write_str("cdr-vocabulary"),
        }
    }
}

impl FromStr for CheckClass {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "origin-platform-header" => Ok(CheckClass::OriginPlatformHeader),
            "cdr-vocabulary" => Ok(CheckClass::CdrVocabulary),
            other => Err(format!("check class {other:?} is not in the closed vocabulary")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_class_round_trips_through_its_token() {
        for token in ["origin-platform-header", "cdr-vocabulary"] {
            let class: CheckClass = token.parse().unwrap();
            assert_eq!(class.to_string(), token);
            assert_eq!(serde_json::to_string(&class).unwrap(), format!("\"{token}\""));
        }
    }

    #[test]
    fn a_class_outside_the_vocabulary_is_refused() {
        assert!("platform-quirk".parse::<CheckClass>().is_err());
        assert!(serde_json::from_str::<CheckClass>("\"platform-quirk\"").is_err());
    }
}
