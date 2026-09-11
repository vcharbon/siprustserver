//! The correlation-rule file (`rule-format-v0.md`): how calls join into cases.
//!
//! This file is the MIDDLE of three layers. In-dialog assembly (Call-ID + tags
//! to dialog, dialogs to legs, B2BUA pairing) is deterministic SIP semantics
//! done before any rule runs. Interpretation — what a chain MEANS — happens
//! after, over the neutral groups. A rule carries NO meaning: its name is an
//! evidence label.
//!
//! Five kinds, each hard-coding its own anchor. The extension path is a SIXTH
//! kind with its own small matcher, never a more general existing one. There is
//! no predicate AST, no boolean composition, no first-wins suppression:
//! ambiguity is reported, never silently resolved.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The `version` this crate models.
pub const RULE_FILE_VERSION: u32 = 0;

/// A correlation rule file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RuleFile {
    /// Always [`RULE_FILE_VERSION`].
    pub version: u32,
    /// The rules, tried in file order.
    pub rules: Vec<Rule>,
}

impl RuleFile {
    /// Parse a rule file from its text.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// The checks the shape alone cannot state: unique names, and every
    /// key-bearing regex carrying its `(?<key>…)` group. Returns one message
    /// per offence.
    pub fn violations(&self) -> Vec<String> {
        let mut seen: Vec<&str> = Vec::new();
        let mut out = Vec::new();
        for rule in &self.rules {
            let name = rule.name();
            if seen.contains(&name) {
                out.push(format!("duplicate rule name {name:?}"));
            }
            seen.push(name);
            for (field, regex) in rule.key_regexes() {
                if !regex.contains("(?<key>") {
                    out.push(format!("rule {name:?}: {field:?} needs a (?<key>…) group"));
                }
            }
        }
        out
    }
}

/// One rule. `name` is unique and shows up in the evidence a join carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Rule {
    /// Derived Call-ID relationship. Anchor: each call's Call-ID, any leg. No
    /// window — the derivation itself is the evidence.
    CallId {
        name: String,
        /// Regex over a left call's Call-ID, yielding the join key.
        left: String,
        /// Regex over a right call's Call-ID, yielding the join key.
        right: String,
    },
    /// Shared token in a header of the initial INVITE. Anchor: each call's
    /// first INVITE.
    HeaderKey {
        name: String,
        header: String,
        /// Regex over the header value, yielding the join key.
        pattern: String,
        /// Maximum gap between the two INVITEs. Absent means no window.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window_ms: Option<u64>,
    },
    /// Reroute / failover chain, CALL-LEVEL: it reads whole-call facts — this
    /// call's terminal failure, that call's start — never individual messages.
    /// A final joins the NEXT candidate INVITE only, so a chain of N attempts
    /// is N-1 ordered joins forming a list.
    Retry {
        name: String,
        /// Status patterns that must terminate the left call: `"486"` exact or
        /// `"5xx"` class.
        finals: Vec<String>,
        window_ms: u64,
        /// OPTIONAL. Absent, the rule fires only between calls ALREADY grouped
        /// by stronger evidence and contributes order and cause alone — so it
        /// never joins strangers. Present, it may itself join, for captures
        /// whose attempts share no other token.
        #[serde(default, rename = "match", skip_serializing_if = "Vec::is_empty")]
        match_on: Vec<MatchField>,
    },
    /// Attended-transfer linkage. Anchor: the right call's initial INVITE
    /// carrying `Replaces` naming the left call's dialog. No fields — the
    /// header is the evidence.
    Replaces { name: String },
    /// REFER-initiated call. Anchor: a REFER on the left call, and the right
    /// call's initial INVITE reaching the `Refer-To` target user.
    Refer { name: String, window_ms: u64 },
}

impl Rule {
    /// The rule's name, whichever kind it is.
    pub fn name(&self) -> &str {
        match self {
            Rule::CallId { name, .. }
            | Rule::HeaderKey { name, .. }
            | Rule::Retry { name, .. }
            | Rule::Replaces { name }
            | Rule::Refer { name, .. } => name,
        }
    }

    /// The rule's key-bearing regex fields, as `(field, regex)`.
    pub fn key_regexes(&self) -> Vec<(&'static str, &str)> {
        match self {
            Rule::CallId { left, right, .. } => {
                vec![("left", left.as_str()), ("right", right.as_str())]
            }
            Rule::HeaderKey { pattern, .. } => vec![("pattern", pattern.as_str())],
            _ => Vec::new(),
        }
    }
}

/// A call-level identity set a `retry` rule may match on. Two calls match when
/// their digits-normalized sets intersect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum MatchField {
    FromUser,
    ToUser,
    RuriUser,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r#"{
      "version": 0,
      "rules": [
        { "name": "derived-callid", "kind": "call-id", "left": "^(?<key>.+)$", "right": "^\\d+-(?<key>.+)$" },
        { "name": "icid", "kind": "header-key", "header": "P-Charging-Vector", "pattern": "icid-value=(?<key>[^;\\s]+)" },
        { "name": "reroute-busy", "kind": "retry", "match": ["to-user"], "finals": ["486", "5xx"], "window_ms": 15000 },
        { "name": "reroute-reject", "kind": "retry", "finals": ["403"], "window_ms": 15000 },
        { "name": "attended-transfer", "kind": "replaces" },
        { "name": "refer-follow", "kind": "refer", "window_ms": 10000 }
      ]
    }"#;

    #[test]
    fn all_five_kinds_decode_and_a_match_less_retry_is_legal() {
        let file = RuleFile::from_json(FILE).unwrap();
        assert_eq!(file.version, RULE_FILE_VERSION);
        assert_eq!(file.rules.len(), 6);
        assert!(file.violations().is_empty());
        let matchless = file.rules.iter().find(|r| r.name() == "reroute-reject").unwrap();
        assert!(matches!(matchless, Rule::Retry { match_on, .. } if match_on.is_empty()));
    }

    #[test]
    fn an_absent_match_stays_absent_through_a_round_trip() {
        let file = RuleFile::from_json(FILE).unwrap();
        let text = crate::canonical::format(&file).unwrap();
        assert!(!text.contains("\"match\": []"));
        assert_eq!(crate::canonical::format(&RuleFile::from_json(&text).unwrap()).unwrap(), text);
    }

    #[test]
    fn a_misspelled_field_is_an_error_not_a_silently_missing_window() {
        let bad = r#"{"version":0,"rules":[{"name":"r","kind":"refer","windows_ms":10000}]}"#;
        assert!(RuleFile::from_json(bad).is_err());
    }

    #[test]
    fn an_unknown_kind_and_a_cross_kind_field_are_both_refused() {
        assert!(
            RuleFile::from_json(r#"{"version":0,"rules":[{"name":"r","kind":"icid"}]}"#).is_err()
        );
        // `finals` belongs to `retry` alone.
        assert!(RuleFile::from_json(
            r#"{"version":0,"rules":[{"name":"r","kind":"refer","window_ms":1,"finals":[]}]}"#
        )
        .is_err());
    }

    #[test]
    fn duplicate_names_and_keyless_regexes_are_reported_not_ignored() {
        let file = RuleFile::from_json(
            r#"{"version":0,"rules":[
                {"name":"r","kind":"call-id","left":"^(.+)$","right":"^(?<key>.+)$"},
                {"name":"r","kind":"replaces"}
            ]}"#,
        )
        .unwrap();
        let found = file.violations();
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().any(|v| v.contains("duplicate rule name")));
        assert!(found.iter().any(|v| v.contains("needs a (?<key>…) group")));
    }
}
