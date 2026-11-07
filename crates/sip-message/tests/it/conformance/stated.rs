//! What a refusal of a grammar-valid value means. A grammar admits forms the RFC
//! text or a stated gate refuses, so such a refusal maps to its source; a refusal
//! of a valid form is a named row, kept per target; any other is an unexplained
//! failure of that target.

/// The refusals a stated rule makes of a form a grammar admits: the text of the
/// refusal, then where the rule is stated. The suite's own grammar generates
/// none of these; the vendored corpus does.
const STATED: &[(&str, &str)] = &[
    (
        "Duplicate From tag",
        "RFC 3261 §7.3.1, rfc3261.txt:1743-1744: a parameter-name MUST NOT appear more than once",
    ),
    (
        "Duplicate To tag",
        "RFC 3261 §7.3.1, rfc3261.txt:1743-1744: a parameter-name MUST NOT appear more than once",
    ),
    ("escaped headers", "ADR-0053 §2; RFC 3261 §19.1.1 Table 1"),
    // The vendored grammars draw a header parameter's name from the URI's
    // `paramchar`; §25.1 has `generic-param = token [ EQUAL gen-value ]`.
    ("parameter name is not a token", "RFC 3261 §25.1, as ADR-0001 holds it"),
    (
        "repeated parameter",
        "RFC 3261 §7.3.1, rfc3261.txt:1743-1744: a parameter-name MUST NOT appear more than once",
    ),
    ("repeated with another value", "RFC 3261 §19.1.1: a URI parameter name appears once"),
    ("written twice", "RFC 3261 §19.1.1: a URI parameter name appears once (ADR-0001, exemptions)"),
    // The grammars generate a request's lines one at a time; a request holds
    // more than any one line states.
    ("Missing From tag", "RFC 3261 §8.1.1.3, ADR-0001: no RFC 2543 compatibility"),
    ("has no Contact", "RFC 3261 §8.1.1.8: a request that can open a dialog names its target"),
    ("Replaces has no to-tag", "RFC 3891 §6.1: exactly one to-tag and one from-tag"),
    ("Replaces has no from-tag", "RFC 3891 §6.1: exactly one to-tag and one from-tag"),
    ("RAck response-num is below 1", "RFC 3262 §7.1: a response-num is 1 to 2^32-1"),
    // RFC 3261's IPv6 rule admits forms RFC 4291 does not; RFC 5954 §4.1
    // replaces it.
    ("is not an IPv6address", "RFC 5954 §4.1, as the URI host reader holds it"),
];

/// The refusals of a valid form the stack still makes: the text of the refusal,
/// then the row that holds it.
const FAILING: &[(&str, &str)] = &[
    ("/2.0\"", "version-case"),
    ("uri-parameter value", "uri-parameter-token-value"),
    ("under its canonical name", "compact-name"),
    ("URI has no scheme: \"*\"", "contact-star-typed"),
    ("Min-SE is not 1*DIGIT", "min-se-parameters"),
    ("Unterminated quoted-string in", "subject-quote"),
    ("holds control byte", "control-byte-after-a-word-dquote"),
    ("Invalid CSeq numeric value", "cseq-past-2-31"),
    ("RSeq value", "rseq-past-2-31"),
    ("RAck response-num is not below 2^31", "rack-past-2-31"),
    ("RAck CSeq-num is not below 2^31", "rack-past-2-31"),
];

/// The row a refusal fails, `None` when a stated rule explains it. A refusal
/// names the header as written, so the match ignores case.
pub fn failing_row(reason: &str) -> Option<&'static str> {
    let lower = reason.to_ascii_lowercase();
    let found = FAILING.iter().find(|(text, _)| lower.contains(&text.to_ascii_lowercase()));
    if let Some((_, row)) = found {
        return Some(row);
    }
    if STATED.iter().any(|(text, _)| reason.contains(text)) {
        return None;
    }
    Some("unexplained")
}
