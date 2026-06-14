//! The Check **field-selector vocabulary**, as the single source the JSON schema
//! draws on for `Check.field` autocomplete. The grammar this mirrors is the one
//! [`crate::checks::extract`] actually evaluates (ADR-0019):
//!
//! - transport endpoints: `source.ip/.port`, `dest.ip/.port`;
//! - the payload: `body`;
//! - any header raw: `header(Name)` (open);
//! - URI-bearing headers `from/to/ruri/pai/ppi/diversion/contact`, list ones
//!   indexable (`pai[1]`), with subfields
//!   `.uri/.userInfo/.host/.port/.displayName/.tag/.param(x)` (R-URI has no
//!   display-name/tag).
//!
//! [`COMMON_FIELDS`] is the curated *ready-to-pick* set surfaced as a JSON-schema
//! `enum` (the dropdown); [`field_schema`] wraps it in an `anyOf` with regex
//! branches so the open forms (`header(X-Foo)`, `pai[1].param(x)`) still validate
//! without being enumerated. `common_fields_are_evaluable` (tests) cross-checks
//! every curated entry against the evaluator so the two never drift.

use serde_json::{Value, json};

/// The curated, ready-to-pick complete selectors offered as completions. Open
/// forms (`header(Name)`, indices, `param(x)`) are *not* listed — they are
/// covered by the regex branches in [`field_schema`]. Keep every entry one the
/// evaluator resolves (guarded by `common_fields_are_evaluable`).
pub const COMMON_FIELDS: &[&str] = &[
    // From / To identity.
    "from.uri",
    "from.userInfo",
    "from.host",
    "from.port",
    "from.displayName",
    "from.tag",
    "to.uri",
    "to.userInfo",
    "to.host",
    "to.port",
    "to.displayName",
    "to.tag",
    // Request-URI (requests only; no display-name/tag).
    "ruri.uri",
    "ruri.userInfo",
    "ruri.host",
    "ruri.port",
    // Asserted / preferred identity + diversion + contact (URI-bearing).
    "pai.uri",
    "pai.userInfo",
    "pai.host",
    "pai.displayName",
    "ppi.uri",
    "ppi.userInfo",
    "ppi.host",
    "diversion.uri",
    "contact.uri",
    // Payload + transport endpoints.
    "body",
    "source.ip",
    "source.port",
    "dest.ip",
    "dest.port",
];

/// A `header(Token)` selector — any header by its raw value. RFC 3261 `token`.
pub const HEADER_PATTERN: &str = r"^header\([A-Za-z0-9!#$%&'*+\-.^_`|~]+\)$";

/// The general URI-header grammar: `<name>[idx]?(.subfield)?`, `subfield` one of
/// the known accessors or `param(x)`. Lets `pai[1].param(x)` etc. validate even
/// though they are not enumerated.
pub const URI_HEADER_PATTERN: &str = concat!(
    r"^(from|to|ruri|pai|ppi|diversion|contact)",
    r"(\[\d+\])?",
    r"(\.(uri|userInfo|host|port|displayName|tag|param\([^)]+\)))?$"
);

/// The JSON-schema fragment for a `Check.field`: the curated `enum` (drives the
/// completion dropdown) OR a regex branch covering the open forms. Authors get
/// suggestions for the common selectors and free validation for the rest.
pub fn field_schema() -> Value {
    json!({
        "description":
            "Field selector over the anchored message. Pick a common one or write \
             header(Name) for any raw header. Grammar: URI headers \
             from/to/ruri/pai/ppi/diversion/contact (list ones indexable, e.g. \
             pai[1]) with .uri/.userInfo/.host/.port/.displayName/.tag/.param(x); \
             body; source.ip/.port; dest.ip/.port.",
        "type": "string",
        "anyOf": [
            { "enum": COMMON_FIELDS },
            { "pattern": HEADER_PATTERN },
            { "pattern": URI_HEADER_PATTERN }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_compile_and_cover_examples() {
        let header = regex::Regex::new(HEADER_PATTERN).unwrap();
        assert!(header.is_match("header(P-Asserted-Identity)"));
        assert!(header.is_match("header(X-Foo)"));
        assert!(!header.is_match("header()"));

        let uri = regex::Regex::new(URI_HEADER_PATTERN).unwrap();
        assert!(uri.is_match("from.userInfo"));
        assert!(uri.is_match("pai[1].param(x)"));
        assert!(uri.is_match("contact"));
        assert!(!uri.is_match("bogus.field"));
    }

    /// The curated enum is a subset of what the evaluator can resolve: every
    /// `COMMON_FIELDS` entry is either a transport/body literal, a `header(..)`,
    /// or matches the URI-header grammar the evaluator parses. A drift here means
    /// the dropdown suggests a selector `checks::extract` would reject.
    #[test]
    fn common_fields_match_the_grammar() {
        let uri = regex::Regex::new(URI_HEADER_PATTERN).unwrap();
        let header = regex::Regex::new(HEADER_PATTERN).unwrap();
        let literals = ["body", "source.ip", "source.port", "dest.ip", "dest.port"];
        for f in COMMON_FIELDS {
            let ok = literals.contains(f) || header.is_match(f) || uri.is_match(f);
            assert!(ok, "curated selector {f:?} is outside the evaluable grammar");
        }
    }
}
