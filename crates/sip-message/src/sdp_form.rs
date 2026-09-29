//! The canonical form of a session description: the attribute order and the
//! explicit direction a stack writes when it serializes a description itself
//! instead of forwarding the author's bytes.
//!
//! Pure and deterministic: no clocks, no randomness, no I/O.

use crate::sdp_doc::{direction_of, media_line, Sections};

/// `sdp` in canonical form, or `None` where it already is one, states no
/// media section, or is not a session description (no `v=` first line).
///
/// The session section stays as written. Each media section is its `m=` line,
/// its other non-attribute lines as written, each format's `a=rtpmap` then its
/// `a=fmtp` in the `m=` format order, the other attributes as written, then
/// the direction attribute (RFC 3264 §6.1). A live media section that states
/// none states the one it takes by default, explicitly: the session-level
/// direction attribute, else `a=sendrecv` (RFC 4566 §6). A section with port
/// 0 gains none: a rejected stream's attributes carry no meaning (RFC 3264
/// §6), and a `bundle-only` section (RFC 8843), also port 0, is read the same
/// way.
///
/// Every line ends with the description's line ending: CRLF where any line
/// uses it (a mix of CRLF and bare LF leaves as CRLF throughout), else LF. A
/// blank line is left out.
pub fn canonical_form(sdp: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(sdp).ok()?;
    if !text.starts_with("v=") {
        return None;
    }
    let sections = Sections::of(text);
    if sections.media.is_empty() {
        return None;
    }
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let session_lines = lines(sections.session);
    let session_direction =
        session_lines.iter().find(|l| is_direction(l)).copied().unwrap_or("a=sendrecv");
    let mut out: Vec<&str> = session_lines;
    for media in &sections.media {
        out.extend(media_in_form(media.text, session_direction));
    }
    let mut form = String::with_capacity(text.len() + 16);
    for line in out {
        form.push_str(line);
        form.push_str(eol);
    }
    (form != text).then(|| form.into_bytes())
}

/// One media section in form: `m=`, the other non-attribute lines, the
/// format attributes by format, the other attributes, the direction.
fn media_in_form<'a>(section: &'a str, session_direction: &'a str) -> Vec<&'a str> {
    let all = lines(section);
    let Some((&m, rest)) = all.split_first() else {
        return all;
    };
    let described = media_line(m.get(2..).unwrap_or_default());
    let formats = described.formats;
    let (attributes, others): (Vec<&str>, Vec<&str>) =
        rest.iter().partition(|l| l.starts_with("a="));
    let mut taken = vec![false; attributes.len()];
    let mut out = vec![m];
    out.extend(others);
    for format in &formats {
        for prefix in ["a=rtpmap:", "a=fmtp:"] {
            for (i, attribute) in attributes.iter().enumerate() {
                if !taken[i] && format_of(attribute, prefix) == Some(format.as_str()) {
                    taken[i] = true;
                    out.push(attribute);
                }
            }
        }
    }
    let directions: Vec<&str> = attributes.iter().copied().filter(|a| is_direction(a)).collect();
    out.extend(
        attributes
            .iter()
            .enumerate()
            .filter(|(i, a)| !taken[*i] && !is_direction(a))
            .map(|(_, a)| *a),
    );
    if !directions.is_empty() {
        out.extend(directions);
    } else if described.port != Some(0) {
        out.push(session_direction);
    }
    out
}

/// The format a `prefix` attribute (`a=rtpmap:` / `a=fmtp:`) names.
fn format_of<'a>(attribute: &'a str, prefix: &str) -> Option<&'a str> {
    attribute.strip_prefix(prefix)?.split_whitespace().next()
}

fn is_direction(line: &str) -> bool {
    line.strip_prefix("a=").and_then(direction_of).is_some()
}

/// The lines of `text`, line endings dropped, blank lines left out.
fn lines(text: &str) -> Vec<&str> {
    text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).filter(|l| !l.is_empty()).collect()
}
