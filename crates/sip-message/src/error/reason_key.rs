//! The key a refusal reason is counted under: the reason with the values it
//! quotes left out, so every message refused for one shape lands on one key.
//!
//! A reason states the broken rule in fixed words and quotes the values it
//! read (`{:?}` or `"…"`, a list or an address in brackets). The key keeps the
//! words, header and parameter names, byte values (`0x3b`) and RFC citations,
//! and writes:
//!
//! - a quoted string as `"…"`, its escapes honoured, an unclosed one to the end;
//! - a bracketed span as `[…]`, quotes inside it honoured;
//! - a number standing alone (digits and dots, between separators) as `N`,
//!   except the number after `RFC`: positions, lengths, ranges, counts, ports
//!   and status codes vary with the message, the rule does not.

/// The words between which a standalone number is looked for.
const SEPARATORS: &[char] = &[' ', '\t', '(', ')', ',', ':', ';', '=', '<', '>', '{', '}'];

/// The key `reason` is counted under (see the module doc).
pub fn reason_key(reason: &str) -> String {
    let mut out = String::with_capacity(reason.len());
    let mut word = String::new();
    let mut previous_word = String::new();
    let mut chars = reason.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                flush(&mut out, &mut word, &mut previous_word);
                skip_quoted(&mut chars);
                out.push_str("\"…\"");
            }
            '[' => {
                flush(&mut out, &mut word, &mut previous_word);
                skip_bracketed(&mut chars);
                out.push_str("[…]");
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

/// Consume a quoted string up to its closing quote, escapes honoured.
fn skip_quoted(chars: &mut std::str::Chars<'_>) {
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '"' => return,
            _ => {}
        }
    }
}

/// Consume a bracketed span up to its matching `]`, quotes inside honoured.
fn skip_bracketed(chars: &mut std::str::Chars<'_>) {
    let mut depth = 1usize;
    while let Some(c) = chars.next() {
        match c {
            '"' => skip_quoted(chars),
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return;
                }
            }
            _ => {}
        }
    }
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

    #[test]
    fn an_unclosed_quote_or_bracket_runs_to_the_end() {
        assert_eq!(reason_key(r#"URI has no scheme: "abc"#), r#"URI has no scheme: "…""#);
        assert_eq!(reason_key("reference [2001:db8"), "reference […]");
    }
}
