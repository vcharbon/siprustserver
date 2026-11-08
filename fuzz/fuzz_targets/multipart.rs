//! A multipart body a peer sends (RFC 2046 §5.1): its parts read under the
//! input's boundary, read off its Content-Type, and composed again. The parts
//! read hold the framing: each one starts after a delimiter line ended by a
//! CRLF and ends at the CRLF of the next delimiter or at the body's end, no
//! content holds a delimiter, nothing is read past the close delimiter, and
//! the parts composed under a boundary of their own read back as themselves.

#![no_main]

use fuzz_lanes::input::multipart_case;
use fuzz_lanes::on_lane_stack;
use libfuzzer_sys::fuzz_target;
use sip_message::{compose_multipart, decompose_multipart, entity_parts};

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    on_lane_stack(move || {
        let (boundary, body) = multipart_case(&data);
        let boundary = String::from_utf8_lossy(boundary);
        let located = decompose_multipart(body, &boundary);
        let spans: Vec<(usize, usize)> = located.iter().map(|p| (p.offset, p.len)).collect();
        framed(body, &boundary, &spans);
        let content_type = format!("multipart/mixed;boundary={boundary}");
        let parts = entity_parts(Some(&content_type), &[], body);
        let Ok(composed) = compose_multipart("multipart/mixed", &parts) else { return };
        let again = entity_parts(Some(&composed.content_type), &[], &composed.body);
        assert_eq!(again, parts, "composed under {:?}, the parts read back", composed.content_type);
    });
});

/// What follows `--boundary` at `at`: the close delimiter, an open one whose
/// line ends at the returned offset (padding and CRLF read, or the body's
/// end), or no delimiter.
#[derive(Debug, PartialEq)]
enum Line {
    Close,
    Open(usize),
    None,
}

fn line_at(body: &[u8], at: usize, boundary: &str) -> Line {
    let dash = [b"--".as_slice(), boundary.as_bytes()].concat();
    if !body.get(at..).is_some_and(|rest| rest.starts_with(&dash)) {
        return Line::None;
    }
    let rest = &body[at + dash.len()..];
    if rest.starts_with(b"--") {
        return Line::Close;
    }
    let padding = rest.iter().take_while(|&&b| b == b' ' || b == b'\t').count();
    match &rest[padding..] {
        [] => Line::Open(body.len()),
        [b'\r', b'\n', ..] => Line::Open(at + dash.len() + padding + 2),
        _ => Line::None,
    }
}

/// The framing the parts `spans` (offset, length) of `body` read under
/// `boundary` hold.
fn framed(body: &[u8], boundary: &str, spans: &[(usize, usize)]) {
    let mut previous_end = None;
    for &(offset, len) in spans {
        let end = offset + len;
        assert!(end <= body.len(), "a part ends inside the body");
        // It ends at the body's end, or the CRLF of the next delimiter.
        if end < body.len() {
            assert!(body[end..].starts_with(b"\r\n"), "a part ends at a CRLF");
            assert_ne!(line_at(body, end + 2, boundary), Line::None, "a delimiter ends a part");
        }
        // It starts after a delimiter line: the one ending the previous part,
        // or the first one of the body.
        let opened_at = match previous_end {
            Some(previous) => previous + 2,
            None => (0..=offset)
                .find(|&at| {
                    (at == 0 || body[..at].ends_with(b"\r\n"))
                        && matches!(line_at(body, at, boundary), Line::Open(_))
                })
                .expect("the first part follows a delimiter"),
        };
        match line_at(body, opened_at, boundary) {
            Line::Open(next) => assert!(next <= offset, "a part starts after its delimiter"),
            other => panic!("a part follows {other:?}: nothing is read past the close"),
        }
        // No content holds a delimiter line of its own.
        let content = &body[offset..end];
        for crlf in
            (0..content.len().saturating_sub(1)).filter(|&i| content[i..].starts_with(b"\r\n"))
        {
            let at = offset + crlf + 2;
            assert_eq!(line_at(body, at, boundary), Line::None, "a part holds a delimiter");
        }
        previous_end = Some(end);
    }
}
