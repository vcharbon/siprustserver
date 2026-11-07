//! What a refusal of a grammar-valid value means. A grammar admits forms the RFC
//! text or a stated gate refuses, so such a refusal maps to its source; a refusal
//! of a valid form is a named row; any other is an unexplained failure.

use super::ratchet::lists;

/// The refusals a stated rule makes of a form its grammar admits: the text of
/// the refusal, then where the rule is stated.
const STATED: &[(&str, &str)] = &[
    ("Duplicate From tag", "ADR-0001, one From tag"),
    ("Duplicate To tag", "RFC 3261 §7.3.1: a parameter name appears once"),
    ("escaped headers", "ADR-0053 §2; RFC 3261 §19.1.1 Table 1"),
    // The vendored grammars draw a header parameter's name from the URI's
    // `paramchar`; §25.1 has `generic-param = token [ EQUAL gen-value ]`.
    ("parameter name is not a token", "RFC 3261 §25.1, as ADR-0001 holds it"),
];

/// The refusals of a valid form the stack still makes: the text of the refusal,
/// then the row that holds it.
const FAILING: &[(&str, &str)] = &[
    ("/2.0\"", "version-case"),
    ("uri-parameter value", "uri-parameter-token-value"),
    ("under its canonical name", "compact-name"),
    ("URI has no scheme: \"*\"", "contact-star-typed"),
    ("Min-SE is not 1*DIGIT", "min-se-parameters"),
];

/// The rare refusals of a valid form a default-lane sweep may not reach, each held
/// by a row of the MUST table while that row is listed.
const HELD_ELSEWHERE: &[(&str, &str)] = &[
    ("unterminated quoted-string in subject", "must/3261-25.1-subject-unbalanced-quote"),
    ("holds control byte", "must/3261-25.1-word-with-a-dquote-then-a-control-byte"),
];

/// The row a refusal fails, `None` when a stated rule explains it.
pub fn failing_row(reason: &str) -> Option<&'static str> {
    if let Some((_, row)) = FAILING.iter().find(|(text, _)| reason.contains(text)) {
        return Some(row);
    }
    let lower = reason.to_ascii_lowercase();
    if HELD_ELSEWHERE.iter().any(|(text, row)| lower.contains(text) && lists(row)) {
        return None;
    }
    if STATED.iter().any(|(text, _)| reason.contains(text)) {
        return None;
    }
    Some("unexplained")
}
