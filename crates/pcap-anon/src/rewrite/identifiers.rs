//! The dialog and transaction identifiers of a datagram, named by field: the
//! value of a `Call-ID`, `In-Reply-To`, `Replaces`, `Join`, `Target-Dialog`
//! or `Session-ID` header (compact `i` included), and the value of a `tag`,
//! `branch`, `call-id`, `from-tag`, `to-tag`, `local-tag`, `remote-tag` or
//! escaped `Replaces` parameter wherever it stands — an `Event` dialog
//! package's, a URI's escaped header — its host part aside (the names
//! pass's, as every host is). The anonymiser maps the words of these
//! identifiers one to one ([`crate::anonymize::identifiers`]) and nothing
//! else touches them, so no two identifiers ever merge.

use std::ops::Range;

use super::fields::header_name;

/// Headers whose whole value is an identifier.
const IDENTIFIER_HEADERS: &[&str] =
    &["call-id", "i", "in-reply-to", "replaces", "join", "target-dialog", "session-id"];

/// Parameters whose value is an identifier.
const IDENTIFIER_PARAMS: &[&str] =
    &["tag", "branch", "call-id", "from-tag", "to-tag", "local-tag", "remote-tag", "replaces"];

/// The magic cookie every RFC 3261 branch opens with: protocol, never an id.
const COOKIE: &[u8] = b"z9hG4bK";

/// Whether `input[start..end]` lies inside an identifier (module doc).
pub(crate) fn is_identifier(input: &[u8], start: usize, end: usize) -> bool {
    let _ = end;
    let line_start = input[..start].iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
    let in_value = input[line_start..start].contains(&b':');
    let whole = in_value
        && header_name(input, start).is_some_and(|n| IDENTIFIER_HEADERS.contains(&n.as_str()));
    whole || param_name(input, start).is_some_and(|n| IDENTIFIER_PARAMS.contains(&n.as_str()))
}

/// Every word of `input` an identifier holds: a run of ASCII letters and
/// digits, the branch cookie left out, at least four bytes long (a shorter
/// word tells no dialog from another and stays as found).
pub(crate) fn words(input: &[u8]) -> Vec<Range<usize>> {
    every_word(input).into_iter().filter(|w| w.len() >= 4).collect()
}

/// Every word shorter than four bytes that opens an identifier's value in
/// `input` (behind a blank, `:` or `=`): it stays as found, and stays the
/// tail of a word a platform glued a prefix onto.
pub(crate) fn short_words(input: &[u8]) -> Vec<Range<usize>> {
    let opens =
        |w: &Range<usize>| w.start > 0 && matches!(input[w.start - 1], b' ' | b'\t' | b':' | b'=');
    every_word(input).into_iter().filter(|w| w.len() < 4 && opens(w)).collect()
}

/// Every word of `input` an identifier holds, whatever its length.
fn every_word(input: &[u8]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    // An address literal is the names pass's, whole.
    let literals = super::addresses::literals(input);
    // The line the scan stands on: where its value starts, and whether its
    // header is an identifier header, read once per line.
    let mut line: Option<(usize, Option<usize>, bool)> = None;
    let mut i = 0;
    while i < input.len() {
        if !input[i].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        let mut start = i;
        while i < input.len() && input[i].is_ascii_alphanumeric() {
            i += 1;
        }
        // A percent escape's two hex digits spell a byte, not the word.
        if start >= 1 && input[start - 1] == b'%' {
            start = (start + 2).min(i);
        }
        if input[start..i].starts_with(COOKIE) {
            start += COOKIE.len();
        }
        if i == start {
            continue;
        }
        let line_start = input[..start].iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
        if line.is_none_or(|(at, _, _)| at != line_start) {
            let end = input[line_start..]
                .iter()
                .position(|b| matches!(b, b'\r' | b'\n'))
                .map_or(input.len(), |p| line_start + p);
            let colon =
                input[line_start..end].iter().position(|b| *b == b':').map(|c| line_start + c);
            let whole = colon.is_some()
                && header_name(input, line_start)
                    .is_some_and(|n| IDENTIFIER_HEADERS.contains(&n.as_str()));
            line = Some((line_start, colon, whole));
        }
        let (_, colon, whole) = line.expect("the line is read");
        let identifier = (whole && colon.is_some_and(|c| c < start))
            || param_name(input, start).is_some_and(|n| IDENTIFIER_PARAMS.contains(&n.as_str()));
        if !identifier {
            continue;
        }
        let host = in_host(input, start);
        let protocol = in_param_name(input, start, i);
        let address = literals.iter().any(|&(s, e)| s <= start && i <= e);
        if !host && !protocol && !address {
            out.push(start..i);
        }
    }
    out
}

/// Whether a word is part of a host (`…@host`, `…%40host`): the names
/// pass's, as every host is. What follows the `@` is a host only where it
/// reads as one ([`is_host_word`]); another `word` (RFC 3261 §25.1) there —
/// a Call-ID a platform derived this one from — is the identifier's own.
fn in_host(input: &[u8], start: usize) -> bool {
    let host_byte = |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-');
    let mut j = start;
    while j > 0 && host_byte(&input[j - 1]) {
        j -= 1;
    }
    let escaped_at = j >= 1 && input[j - 1] == b'%' && input[j..].starts_with(b"40");
    let end = start + input[start..].iter().take_while(|b| host_byte(b)).count();
    ((j >= 1 && input[j - 1] == b'@') || escaped_at) && is_host_word(&input[j..end])
}

/// Whether the part of an identifier behind its `@` reads as a host: dotted,
/// or undotted with no word of four bytes or more (`pbx`, `gw1-7`), which no
/// identifier pass would map. An undotted part holding such a word is a
/// Call-ID a server derived this one from.
pub(crate) fn is_host_word(part: &[u8]) -> bool {
    part.contains(&b'.') || part.split(|b| !b.is_ascii_alphanumeric()).all(|w| w.len() < 4)
}

/// Whether the word `input[start..end]` is part of a parameter name (`;remote=`,
/// `%3Bfrom-tag%3D`): protocol, never an identifier.
fn in_param_name(input: &[u8], start: usize, end: usize) -> bool {
    let name_byte = |b: &u8| b.is_ascii_alphanumeric() || *b == b'-';
    let mut s = start;
    while s > 0 && name_byte(&input[s - 1]) && !(s >= 3 && escape_at(input, s - 3, &[b"%3B"])) {
        s -= 1;
    }
    let mut e = end;
    while e < input.len() && name_byte(&input[e]) && !escape_at(input, e, &[b"%3D"]) {
        e += 1;
    }
    let opens = (s > 0 && input[s - 1] == b';') || (s >= 3 && escape_at(input, s - 3, &[b"%3B"]));
    let closes = input.get(e) == Some(&b'=') || escape_at(input, e, &[b"%3D"]);
    opens && closes
}

/// Whether `input[at..]` opens one of the given percent escapes.
fn escape_at(input: &[u8], at: usize, escapes: &[&[u8]]) -> bool {
    escapes.iter().any(|e| input.get(at..at + 3).is_some_and(|s| s.eq_ignore_ascii_case(e)))
}

/// The name, lowercase, of the parameter whose value `input[at]` stands in:
/// back over the value to its `=` (or `%3D`), then over the name to its `;`,
/// `?` or `&` (or their escapes).
pub(crate) fn param_name(input: &[u8], at: usize) -> Option<String> {
    let stop = |b: u8| {
        matches!(b, b';' | b'?' | b'&' | b' ' | b'\t' | b'<' | b'>' | b',' | b'"' | b'\r' | b'\n')
    };
    let mut i = at;
    let eq = loop {
        if i == 0 {
            return None;
        }
        i -= 1;
        if input[i] == b'=' {
            break i;
        }
        if stop(input[i]) || escape_at(input, i, &[b"%3B", b"%26", b"%3F"]) {
            return None;
        }
        if escape_at(input, i, &[b"%3D"]) {
            break i;
        }
    };
    let mut j = eq;
    let separator = |j: usize| j >= 3 && escape_at(input, j - 3, &[b"%3B", b"%26", b"%3F"]);
    while j > 0
        && !separator(j)
        && (input[j - 1].is_ascii_alphanumeric() || matches!(input[j - 1], b'-' | b'.' | b'+'))
    {
        j -= 1;
    }
    let opens = j == 0 || stop(input[j - 1]) || separator(j);
    let name = &input[j..eq];
    (opens && !name.is_empty()).then(|| String::from_utf8_lossy(name).to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words_of(text: &str) -> Vec<&str> {
        words(text.as_bytes()).into_iter().map(|r| &text[r]).collect()
    }

    #[test]
    fn the_words_of_every_identifier_are_found_and_nothing_else() {
        let head = "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP h.example.com:5060;branch=z9hG4bK7a7a7a7a.1;rport=5061\r\n\
                    From: <sip:4444@h.example.com>;tag=5b83-0003-AbCd\r\nCall-ID: 9f9f9f9f-77@h.example.com\r\n\
                    Event: dialog;call-id=9f9f9f9f-77@h.example.com;from-tag=5b83\r\n\
                    Refer-To: <sip:b@h.example.com?Replaces=abcdef%40h.example.com%3Bto-tag%3D12345678>\r\n\
                    Session-ID: 0123456789abcdef0123456789abcdef\r\nX-Other: 99999999\r\n\r\n";
        assert_eq!(
            words_of(head),
            [
                "7a7a7a7a",
                "5b83",
                "0003",
                "AbCd",
                "9f9f9f9f",
                "9f9f9f9f",
                "5b83",
                "abcdef",
                "12345678",
                "0123456789abcdef0123456789abcdef"
            ]
        );
    }
}
