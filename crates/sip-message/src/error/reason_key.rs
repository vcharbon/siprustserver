//! The key a refusal reason is counted under: the reason with the values it
//! quotes left out, so every message refused for one shape lands on one key.
//!
//! The contract with the reason's writers: a reason states the broken rule in
//! fixed words, names a header by its canonical name, and writes every value it
//! read from the message through `{:?}`, a Rust debug string. The key keeps the
//! words, header names, byte values (`0x3b`) and RFC citations, and writes:
//!
//! - a quoted string as `"…"`;
//! - a bracketed span as `[…]`, quotes inside it read as above;
//! - a number standing alone (digits and dots, between separators) as `N`,
//!   except the number after `RFC`: positions, lengths, ranges, counts, ports
//!   and status codes vary with the message, the rule does not.
//!
//! It fails closed: a quoted span that is no debug string (an escape debug
//! never writes, a raw control character, no closing quote, or a closing quote
//! followed by a word character, the mark of a raw `"` inside a value) ends the
//! key at `"…"`, so a writer breaking the contract loses its shape, never leaks
//! the value.

/// The words between which a standalone number is looked for.
const SEPARATORS: &[char] = &[' ', '\t', '(', ')', ',', ':', ';', '=', '<', '>', '{', '}'];

/// What may follow a debug string's closing quote in a reason.
const AFTER_QUOTE: &[char] = &[' ', ')', ',', ':', ';', ']', '\'', '.'];

/// The key `reason` is counted under (see the module doc).
pub fn reason_key(reason: &str) -> String {
    let mut out = String::with_capacity(reason.len());
    let mut word = String::new();
    let mut previous_word = String::new();
    let mut chars = reason.chars();
    // Each step takes one character: at most `len + 1` (ADR-0057 §5).
    for _ in 0..=reason.len() {
        let Some(c) = chars.next() else { break };
        match c {
            '"' => {
                flush(&mut out, &mut word, &mut previous_word);
                out.push_str("\"…\"");
                if !skip_debug_string(&mut chars) {
                    return out;
                }
            }
            '[' => {
                flush(&mut out, &mut word, &mut previous_word);
                out.push_str("[…]");
                if !skip_bracketed(&mut chars) {
                    return out;
                }
            }
            c if SEPARATORS.contains(&c) => {
                flush(&mut out, &mut word, &mut previous_word);
                out.push(c);
            }
            c => word.push(c),
        }
    }
    flush(&mut out, &mut word, &mut previous_word);
    out
}

/// Write `word` to `out`, as `N` when it is a number no `RFC` names.
fn flush(out: &mut String, word: &mut String, previous_word: &mut String) {
    if word.is_empty() {
        return;
    }
    let number = word.chars().any(|c| c.is_ascii_digit())
        && word.chars().all(|c| c.is_ascii_digit() || c == '.');
    if number && previous_word != "RFC" {
        out.push('N');
    } else {
        out.push_str(word);
    }
    *previous_word = std::mem::take(word);
}

/// Consume a debug string after its opening quote, up to its closing quote;
/// `false` when what follows is no debug string (see the module doc).
fn skip_debug_string(chars: &mut std::str::Chars<'_>) -> bool {
    let left = chars.as_str().len();
    for _ in 0..=left {
        let Some(c) = chars.next() else { break };
        match c {
            '\\' => match chars.next() {
                Some('\\' | '"' | '\'' | 'n' | 'r' | 't' | '0') => {}
                Some('u') => {
                    if !skip_unicode_escape(chars) {
                        return false;
                    }
                }
                _ => return false,
            },
            '"' => return chars.clone().next().is_none_or(|next| AFTER_QUOTE.contains(&next)),
            c if c.is_control() => return false,
            _ => {}
        }
    }
    false
}

/// Consume the `{hex}` of a `\u{…}` escape; `false` when it is not one.
fn skip_unicode_escape(chars: &mut std::str::Chars<'_>) -> bool {
    if chars.next() != Some('{') {
        return false;
    }
    let mut digits = 0usize;
    for c in chars.by_ref() {
        match c {
            '}' => return (1..=6).contains(&digits),
            c if c.is_ascii_hexdigit() => digits = digits.saturating_add(1),
            _ => return false,
        }
    }
    false
}

/// Consume a bracketed span up to its matching `]`, debug strings inside read
/// as such; `false` when one of them is none or the span never closes.
fn skip_bracketed(chars: &mut std::str::Chars<'_>) -> bool {
    let mut depth = 1usize;
    let left = chars.as_str().len();
    for _ in 0..=left {
        let Some(c) = chars.next() else { break };
        match c {
            '"' => {
                if !skip_debug_string(chars) {
                    return false;
                }
            }
            '[' => depth = depth.saturating_add(1),
            ']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::reason_key;

    #[test]
    fn a_reason_with_no_value_is_its_own_key() {
        let reason = "INVITE has no Contact (RFC 3261 §8.1.1.8)";
        assert_eq!(reason_key(reason), reason);
        let reason = "Multiple Server headers (RFC 3261 §7.3.1 — one value)";
        assert_eq!(reason_key(reason), reason);
    }

    #[test]
    fn quoted_values_are_left_out_whatever_they_hold() {
        let one = r#"Strict Contact: URI parameter "x-a" written twice (RFC 3261 §19.1.1) ("<sip:alice@192.0.2.1;x-a=1;x-a=1>")"#;
        let other = r#"Strict Contact: URI parameter "x-b" written twice (RFC 3261 §19.1.1) ("<sip:bob@198.51.100.7:5061;x-b=\"q\";x-b=\"q\">")"#;
        let key = r#"Strict Contact: URI parameter "…" written twice (RFC 3261 §19.1.1) ("…")"#;
        assert_eq!(reason_key(one), key);
        assert_eq!(reason_key(other), key);
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_value() {
        let reason =
            r#"byte 0x3b not allowed in "a;text=\"Busy\"" in URI "sip:b@h?R=a;text=\"Busy\"""#;
        assert_eq!(reason_key(reason), r#"byte 0x3b not allowed in "…" in URI "…""#);
    }

    #[test]
    fn standalone_numbers_are_n_and_rfc_numbers_and_glued_digits_stay() {
        assert_eq!(reason_key("Expected CRLF at position 1234"), "Expected CRLF at position N");
        assert_eq!(reason_key("body 40..812 past the datagram"), "body N past the datagram");
        assert_eq!(
            reason_key("Non-100 response (status=180) missing mandatory To-tag"),
            "Non-100 response (status=N) missing mandatory To-tag"
        );
        assert_eq!(
            reason_key("header block is not UTF-8 (RFC 3629) at byte 17, in From"),
            "header block is not UTF-8 (RFC 3629) at byte N, in From"
        );
        assert_eq!(
            reason_key("Geolocation-Error code is not 1*3DIGIT: IPv6 0x20"),
            "Geolocation-Error code is not 1*3DIGIT: IPv6 0x20"
        );
    }

    #[test]
    fn bracketed_spans_are_left_out() {
        assert_eq!(
            reason_key("invalid IPv6 reference [2001:db8::1]"),
            "invalid IPv6 reference […]"
        );
        assert_eq!(
            reason_key(r#"leading token is none of ["a]", "b"]"#),
            "leading token is none of […]"
        );
    }

    /// A value written between raw quotes holding a `"` of its own ends the
    /// key at that value: nothing of it, nor anything after it, is kept.
    #[test]
    fn a_raw_quoted_value_holding_a_quote_ends_the_key() {
        let reason = r#"Strict Contact: bad (""Fakey McFakename" <sip:fakey@192.0.2.5>") tail"#;
        let key = reason_key(reason);
        assert_eq!(key, r#"Strict Contact: bad ("…""#);
        for leaked in ["Fakey", "McFakename", "fakey", "192"] {
            assert!(!key.contains(leaked), "{key}");
        }
        assert_eq!(reason_key(r#"in "a"McFakename""#), r#"in "…""#);
    }

    #[test]
    fn an_escape_or_byte_no_debug_string_holds_ends_the_key() {
        assert_eq!(reason_key(r#"value "a\q McFakename" tail"#), r#"value "…""#);
        assert_eq!(reason_key("value \"a\u{7}b\" tail"), r#"value "…""#);
        assert_eq!(reason_key("value \"a\tb\" tail"), r#"value "…""#);
        assert_eq!(reason_key(r#"value "a\u{e9}\tb" tail"#), r#"value "…" tail"#);
        assert_eq!(reason_key(r#"value "a\u{zz}" tail"#), r#"value "…""#);
    }

    #[test]
    fn an_unclosed_quote_or_bracket_runs_to_the_end() {
        assert_eq!(reason_key(r#"URI has no scheme: "abc"#), r#"URI has no scheme: "…""#);
        assert_eq!(reason_key("reference [2001:db8"), "reference […]");
    }
}
