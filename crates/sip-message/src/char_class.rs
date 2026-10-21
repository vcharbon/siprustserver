//! The character classes of RFC 3261 §25.1, one predicate per class, over
//! bytes.
//!
//! Every class here is ASCII. `UTF8-NONASCII` is legal only inside a
//! quoted-string, a comment, a reason phrase and TEXT-UTF8; no reader of a
//! URI, token or word ever admits a byte of 0x80 or above.

/// `alphanum = ALPHA / DIGIT`.
pub fn is_alphanum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

/// `mark = "-" / "_" / "." / "!" / "~" / "*" / "'" / "(" / ")"`.
pub fn is_mark(b: u8) -> bool {
    matches!(b, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')')
}

/// `unreserved = alphanum / mark`.
pub fn is_unreserved(b: u8) -> bool {
    is_alphanum(b) || is_mark(b)
}

/// `reserved = ";" / "/" / "?" / ":" / "@" / "&" / "=" / "+" / "$" / ","`.
pub fn is_reserved(b: u8) -> bool {
    matches!(b, b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',')
}

/// `user-unreserved = "&" / "=" / "+" / "$" / "," / ";" / "?" / "/"`.
pub fn is_user_unreserved(b: u8) -> bool {
    matches!(b, b'&' | b'=' | b'+' | b'$' | b',' | b';' | b'?' | b'/')
}

/// The unescaped bytes of `user = 1*( unreserved / escaped / user-unreserved )`.
pub fn is_user_char(b: u8) -> bool {
    is_unreserved(b) || is_user_unreserved(b)
}

/// The unescaped bytes of
/// `password = *( unreserved / escaped / "&" / "=" / "+" / "$" / "," )`.
pub fn is_password_char(b: u8) -> bool {
    is_unreserved(b) || matches!(b, b'&' | b'=' | b'+' | b'$' | b',')
}

/// `param-unreserved = "[" / "]" / "/" / ":" / "&" / "+" / "$"`.
pub fn is_param_unreserved(b: u8) -> bool {
    matches!(b, b'[' | b']' | b'/' | b':' | b'&' | b'+' | b'$')
}

/// The unescaped bytes of `paramchar = param-unreserved / unreserved / escaped`.
pub fn is_paramchar(b: u8) -> bool {
    is_param_unreserved(b) || is_unreserved(b)
}

/// `hnv-unreserved = "[" / "]" / "/" / "?" / ":" / "+" / "$"`.
pub fn is_hnv_unreserved(b: u8) -> bool {
    matches!(b, b'[' | b']' | b'/' | b'?' | b':' | b'+' | b'$')
}

/// The unescaped bytes of an escaped-header name or value:
/// `hnv-unreserved / unreserved / escaped`.
pub fn is_hnv_char(b: u8) -> bool {
    is_hnv_unreserved(b) || is_unreserved(b)
}

/// `token = 1*(alphanum / "-" / "." / "!" / "%" / "*" / "_" / "+" / "`" / "'" / "~" )`.
pub fn is_token_char(b: u8) -> bool {
    is_alphanum(b)
        || matches!(b, b'-' | b'.' | b'!' | b'%' | b'*' | b'_' | b'+' | b'`' | b'\'' | b'~')
}

/// `word`: the token characters plus `( ) < > : \ DQUOTE / [ ] ? { }`.
pub fn is_word_char(b: u8) -> bool {
    is_token_char(b)
        || matches!(
            b,
            b'(' | b')'
                | b'<'
                | b'>'
                | b':'
                | b'\\'
                | b'"'
                | b'/'
                | b'['
                | b']'
                | b'?'
                | b'{'
                | b'}'
        )
}

/// A byte of the URI character set every scheme shares (`reserved /
/// unreserved / "%"`, plus the brackets of an IPv6 reference): what a URI of
/// a scheme this stack does not read by its own grammar may carry.
pub fn is_uri_char(b: u8) -> bool {
    is_reserved(b) || is_unreserved(b) || matches!(b, b'%' | b'[' | b']')
}

/// A byte of `UTF8-NONASCII`: the lead or continuation byte of a non-ASCII
/// character.
pub fn is_utf8_nonascii(b: u8) -> bool {
    b >= 0x80
}

/// Whether `text` is one or more bytes of `class`, an `escaped` triplet
/// (`"%" HEXDIG HEXDIG`) standing for any byte where `escapes` is true.
pub fn is_run_of(text: &str, class: fn(u8) -> bool, escapes: bool) -> bool {
    !text.is_empty() && run_refusal(text, class, escapes).is_none()
}

/// The first byte of `text` that is neither of `class` nor (where `escapes`
/// is true) part of a well-formed `escaped` triplet, as a reason; `None` when
/// every byte belongs.
pub fn run_refusal(text: &str, class: fn(u8) -> bool, escapes: bool) -> Option<String> {
    let bytes = text.as_bytes();
    let mut rest = bytes;
    while let Some((&b, tail)) = rest.split_first() {
        if escapes && b == b'%' {
            match tail {
                [h, l, more @ ..] if h.is_ascii_hexdigit() && l.is_ascii_hexdigit() => {
                    rest = more;
                    continue;
                }
                _ => return Some(format!("malformed escape in {text:?}")),
            }
        }
        if !class(b) {
            return Some(format!("byte 0x{b:02x} not allowed in {text:?}"));
        }
        rest = tail;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_class_admits_a_non_ascii_byte() {
        let classes: [fn(u8) -> bool; 9] = [
            is_unreserved,
            is_reserved,
            is_user_char,
            is_password_char,
            is_paramchar,
            is_hnv_char,
            is_token_char,
            is_word_char,
            is_uri_char,
        ];
        for class in classes {
            assert!((0x80..=0xff).all(|b| !class(b)));
            assert!((0x00..0x20).all(|b| !class(b)), "no control byte");
            assert!(!class(b' ') && !class(0x7f));
        }
        assert!((0x80..=0xff).all(is_utf8_nonascii));
    }

    #[test]
    fn an_escape_stands_for_one_byte_and_must_be_whole() {
        assert!(is_run_of("a%C3%ABb", is_user_char, true));
        assert!(is_run_of("%00", is_user_char, true));
        for bad in ["%G1", "a%", "%0", "%%41"] {
            assert!(!is_run_of(bad, is_user_char, true), "{bad}");
        }
        assert!(is_run_of("%4", is_token_char, false), "a token's % is a byte, no escape");
        assert!(!is_run_of("", is_token_char, false), "a run is one or more");
    }
}
