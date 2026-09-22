//! Request matching: target (method + path, query cut off) and body
//! fragments, with `${capture:…}` as the one wildcard.
//!
//! The body is compact-re-serialised when it parses as JSON, so a
//! pretty-printing peer matches the same fragments; otherwise it is matched
//! as (lossily decoded) text.

use super::program::{HttpBindings, HttpRequestMatch};
use super::template::{self, Captures, Piece};
use crate::HttpRequest;

/// The longest text one capture takes.
pub(super) const CAPTURE_CAP: usize = 4096;

/// The body as fragments are matched against it.
pub(super) fn normalize(body: &[u8]) -> String {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(value) => value.to_string(),
        Err(_) => String::from_utf8_lossy(body).into_owned(),
    }
}

fn path_only(path: &str) -> &str {
    path.split_once('?').map_or(path, |(p, _)| p)
}

/// Whether `req` has the method and path `m` states.
pub(super) fn same_target(m: &HttpRequestMatch, req: &HttpRequest) -> bool {
    m.method == req.method && path_only(&m.path) == path_only(&req.path)
}

/// Match `req` (its normalised `body`) against `m`, extending `captures`.
/// The error says which fragment is missing, expected against received.
pub(super) fn matches(
    m: &HttpRequestMatch,
    bindings: &HttpBindings,
    req: &HttpRequest,
    body: &str,
    captures: &mut Captures,
) -> Result<(), String> {
    if !same_target(m, req) {
        return Err(format!(
            "expected {} {}, received {} {}",
            m.method,
            path_only(&m.path),
            req.method,
            path_only(&req.path)
        ));
    }
    let mut taken = captures.clone();
    for entry in &m.contains {
        let shown = template::show(entry, bindings);
        let pieces = template::parse(entry).map_err(|t| format!("bad fragment {t}"))?;
        match find(&pieces, bindings, body, &mut taken) {
            Ok(()) => {}
            Err(Miss::Absent) => {
                return Err(format!(
                    "expected {} {} containing {shown}; received body {body}",
                    m.method,
                    path_only(&m.path)
                ));
            }
            Err(Miss::OverCap(name)) => {
                return Err(format!(
                    "${{capture:{name}}} in {shown} exceeds {CAPTURE_CAP} bytes; received body {body}"
                ));
            }
        }
    }
    *captures = taken;
    Ok(())
}

enum Miss {
    Absent,
    OverCap(String),
}

enum Seg {
    Lit(String),
    Hole { name: String, in_string: bool },
}

/// Locate one fragment in `body`. A capture already taken matches as its
/// value; an untaken one matches one JSON scalar: the content of a string
/// when the fragment has an open quote before it, a number or literal up to
/// the next `,` `}` `]` otherwise.
fn find(
    pieces: &[Piece],
    bindings: &HttpBindings,
    body: &str,
    captures: &mut Captures,
) -> Result<(), Miss> {
    let segs = segments(pieces, bindings, captures);
    let Some(Seg::Lit(first)) = segs.first() else {
        // Refused at `add`: a capture must follow literal text.
        return Err(Miss::Absent);
    };
    let mut over_cap = None;
    for (start, _) in body.match_indices(first.as_str()) {
        let mut taken = Vec::new();
        match rest_matches(&segs[1..], body, start + first.len(), &mut taken) {
            Ok(true) => {
                captures.extend(taken);
                return Ok(());
            }
            Ok(false) => {}
            Err(name) => over_cap = Some(name),
        }
    }
    Err(over_cap.map_or(Miss::Absent, Miss::OverCap))
}

fn segments(pieces: &[Piece], bindings: &HttpBindings, captures: &Captures) -> Vec<Seg> {
    let mut segs: Vec<Seg> = Vec::new();
    let mut quotes = 0usize;
    for piece in pieces {
        match piece {
            Piece::Lit(text) => push_lit(&mut segs, text, &mut quotes),
            Piece::Bind(name) => push_lit(&mut segs, bindings.get(name).unwrap_or(""), &mut quotes),
            Piece::Capture(name) => match captures.get(name) {
                Some(value) => push_lit(&mut segs, value, &mut quotes),
                None => segs.push(Seg::Hole { name: name.clone(), in_string: quotes % 2 == 1 }),
            },
            // Refused in a match at `add`.
            Piece::Continuation => {}
        }
    }
    segs
}

/// Append literal text, merged into a preceding literal; `quotes` counts the
/// unescaped quotes seen, whose parity says whether a hole sits in a string.
fn push_lit(segs: &mut Vec<Seg>, text: &str, quotes: &mut usize) {
    *quotes += unescaped_quotes(text);
    match segs.last_mut() {
        Some(Seg::Lit(prev)) => prev.push_str(text),
        _ => segs.push(Seg::Lit(text.to_string())),
    }
}

fn unescaped_quotes(text: &str) -> usize {
    let bytes = text.as_bytes();
    (0..bytes.len()).filter(|&i| bytes[i] == b'"' && !escaped(bytes, 0, i)).count()
}

/// Whether the byte at `i` is preceded by an odd run of backslashes that
/// starts no earlier than `from`.
fn escaped(bytes: &[u8], from: usize, i: usize) -> bool {
    let mut n = 0;
    let mut j = i;
    while j > from && bytes[j - 1] == b'\\' {
        n += 1;
        j -= 1;
    }
    n % 2 == 1
}

/// Where the scalar starting at `pos` ends at the latest.
fn boundary(body: &str, pos: usize, in_string: bool) -> Option<usize> {
    let bytes = body.as_bytes();
    if in_string {
        (pos..bytes.len()).find(|&i| bytes[i] == b'"' && !escaped(bytes, pos, i))
    } else {
        Some((pos..bytes.len()).find(|&i| b",}]".contains(&bytes[i])).unwrap_or(bytes.len()))
    }
}

/// Match the segments after the first literal from `pos`. `Err` names a
/// capture that would exceed [`CAPTURE_CAP`].
fn rest_matches(
    segs: &[Seg],
    body: &str,
    mut pos: usize,
    taken: &mut Vec<(String, String)>,
) -> Result<bool, String> {
    let mut i = 0;
    while i < segs.len() {
        match &segs[i] {
            Seg::Lit(text) => {
                if !body[pos..].starts_with(text.as_str()) {
                    return Ok(false);
                }
                pos += text.len();
                i += 1;
            }
            Seg::Hole { name, in_string } => {
                let Some(limit) = boundary(body, pos, *in_string) else {
                    return Ok(false);
                };
                let end = match segs.get(i + 1) {
                    // The earliest occurrence of the next literal within the
                    // scalar that does not split an escape sequence.
                    Some(Seg::Lit(next)) => match body[pos..]
                        .match_indices(next.as_str())
                        .map(|(offset, _)| pos + offset)
                        .take_while(|&q| q <= limit)
                        .find(|&q| !escaped(body.as_bytes(), pos, q))
                    {
                        Some(q) => q,
                        None => return Ok(false),
                    },
                    _ => limit,
                };
                if !*in_string && end == pos {
                    return Ok(false);
                }
                if end - pos > CAPTURE_CAP {
                    return Err(name.clone());
                }
                taken.push((name.clone(), body[pos..end].to_string()));
                pos = end;
                i += 1;
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(entry: &str, body: &str) -> Result<Captures, ()> {
        let mut captures = Captures::new();
        let pieces = template::parse(entry).unwrap();
        find(&pieces, &HttpBindings::new(), body, &mut captures).map_err(|_| ())?;
        Ok(captures)
    }

    #[test]
    fn a_string_capture_takes_the_escaped_content_up_to_the_closing_quote() {
        let got = capture(r#""id":"${capture:id}""#, r#"{"id":"a\"b\\","n":1}"#).unwrap();
        assert_eq!(got["id"], r#"a\"b\\"#);
    }

    #[test]
    fn a_scalar_capture_stops_at_the_next_delimiter() {
        assert_eq!(capture(r#""n":${capture:n}"#, r#"{"n":-1.5e3,"m":2}"#).unwrap()["n"], "-1.5e3");
        assert_eq!(capture(r#""n":${capture:n}"#, r#"{"m":[1],"n":true}"#).unwrap()["n"], "true");
        assert!(capture(r#""n":${capture:n}"#, r#"{"n":}"#).is_err(), "a scalar is never empty");
    }

    #[test]
    fn a_capture_inside_a_string_stops_before_the_following_literal() {
        let got =
            capture(r#""to":"sip:${capture:user}@host""#, r#"{"to":"sip:bob@host"}"#).unwrap();
        assert_eq!(got["user"], "bob");
        assert!(
            capture(r#""to":"sip:${capture:user}@host""#, r#"{"to":"sip:bob","x":"@host"}"#)
                .is_err(),
            "a capture never crosses its scalar"
        );
    }

    #[test]
    fn a_later_occurrence_is_tried_when_the_first_fails() {
        let got = capture(r#""k":"${capture:v}"x"#, r#"{"k":"a","z":{"k":"b"x"}}"#);
        assert_eq!(got.unwrap()["v"], "b");
    }

    #[test]
    fn a_capture_over_the_cap_is_a_miss() {
        let body = format!(r#"{{"id":"{}"}}"#, "x".repeat(CAPTURE_CAP + 1));
        let pieces = template::parse(r#""id":"${capture:id}""#).unwrap();
        let miss = find(&pieces, &HttpBindings::new(), &body, &mut Captures::new());
        assert!(matches!(miss, Err(Miss::OverCap(name)) if name == "id"));
    }

    #[test]
    fn json_bodies_are_compacted_and_other_bodies_kept() {
        assert_eq!(normalize(b"{ \"a\" : [ 1 , 2 ] }"), r#"{"a":[1,2]}"#);
        assert_eq!(normalize(b"a=1&b=2"), "a=1&b=2");
    }
}
