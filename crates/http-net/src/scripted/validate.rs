//! The checks `add` runs: a script it accepts can be served as written.

use std::collections::HashSet;

use super::error::HttpScriptError;
use super::open::Fragment;
use super::program::{HttpBindings, HttpReply, HttpRequestMatch, HttpScript, HttpScriptStep};
use super::template::{self, Piece};

/// What `add` keeps of a checked script's open.
pub(super) struct Checked {
    /// The fragments an opening request must carry (the open's, and step 0's
    /// for a reified script): the open-match rule's set.
    pub fragments: Vec<Fragment>,
    /// Whether the open states a `${bind:…}` fragment.
    pub attributable: bool,
}

pub(super) fn check(
    script: &HttpScript,
    bindings: &HttpBindings,
) -> Result<Checked, HttpScriptError> {
    let mut captured = HashSet::new();
    check_match(&script.open, bindings, "open", &mut captured)?;
    if let HttpScriptStep::Reified(steps) = &script.step {
        if steps.is_empty() {
            return Err(HttpScriptError::NoStep);
        }
        let target = |m: &HttpRequestMatch| {
            format!("{} {}", m.method, m.path.split_once('?').map_or(m.path.as_str(), |(p, _)| p))
        };
        if target(&steps[0].expect) != target(&script.open) {
            return Err(HttpScriptError::StepZeroTarget {
                open: target(&script.open),
                expected: target(&steps[0].expect),
            });
        }
        let mut previous_mints = true;
        for (index, step) in steps.iter().enumerate() {
            if !previous_mints {
                return Err(HttpScriptError::UnreachableStep { index, previous: index - 1 });
            }
            check_match(&step.expect, bindings, &format!("step {index} expect"), &mut captured)?;
            previous_mints =
                check_reply(&step.reply, bindings, &format!("step {index} reply"), &captured)?;
        }
    }
    let attributable = script.open.contains.iter().any(|entry| {
        template::parse(entry)
            .is_ok_and(|pieces| pieces.iter().any(|p| matches!(p, Piece::Bind(_))))
    });
    let step_zero = match &script.step {
        HttpScriptStep::Reified(steps) => steps[0].expect.contains.as_slice(),
        HttpScriptStep::Code(_) => &[],
    };
    let fragments =
        script.open.contains.iter().chain(step_zero).map(|e| Fragment::new(e, bindings)).collect();
    Ok(Checked { fragments, attributable })
}

fn check_match(
    m: &HttpRequestMatch,
    bindings: &HttpBindings,
    at: &str,
    captured: &mut HashSet<String>,
) -> Result<(), HttpScriptError> {
    if m.method.is_empty() || m.path.is_empty() {
        return Err(HttpScriptError::EmptyTarget { at: at.to_string() });
    }
    for entry in &m.contains {
        let pieces = parse(entry, at)?;
        let mut in_entry = HashSet::new();
        for (i, piece) in pieces.iter().enumerate() {
            match piece {
                Piece::Lit(_) => {}
                Piece::Bind(name) => bound(bindings, name, at)?,
                Piece::Continuation => {
                    return Err(HttpScriptError::ContinuationInMatch { at: at.to_string() });
                }
                Piece::Capture(name) => {
                    let anchored = i > 0 && !matches!(pieces[i - 1], Piece::Capture(_));
                    if !anchored || !in_entry.insert(name.clone()) {
                        return Err(HttpScriptError::UnanchoredCapture {
                            at: at.to_string(),
                            name: name.clone(),
                        });
                    }
                }
            }
        }
        captured.extend(in_entry);
    }
    Ok(())
}

/// Check one reply; `true` when it mints a token (a response whose header
/// values or body carry `${continuation}`).
fn check_reply(
    reply: &HttpReply,
    bindings: &HttpBindings,
    at: &str,
    captured: &HashSet<String>,
) -> Result<bool, HttpScriptError> {
    match reply {
        HttpReply::Silence | HttpReply::Reset => Ok(false),
        HttpReply::Late { then, .. } => check_reply(then, bindings, at, captured),
        HttpReply::Respond { status, headers, body } => {
            if !final_status(*status) {
                return Err(HttpScriptError::InvalidStatus { at: at.to_string(), status: *status });
            }
            for (name, value) in headers {
                let invalid = |why: String| HttpScriptError::InvalidHeader {
                    at: at.to_string(),
                    name: name.clone(),
                    why,
                };
                header_name(name).map_err(invalid)?;
                // Literal text and bound values are known now; a capture is
                // checked when the reply renders.
                for piece in parse(value, at)? {
                    let known = match &piece {
                        Piece::Lit(text) => text.as_str(),
                        Piece::Bind(name) => bindings.get(name).unwrap_or(""),
                        Piece::Capture(_) | Piece::Continuation => "",
                    };
                    header_value(known).map_err(invalid)?;
                }
            }
            let mut mints = false;
            for text in headers.iter().map(|(_, v)| v).chain(std::iter::once(body)) {
                for piece in parse(text, at)? {
                    match piece {
                        Piece::Lit(_) => {}
                        Piece::Bind(name) => bound(bindings, &name, at)?,
                        Piece::Capture(name) if !captured.contains(&name) => {
                            return Err(HttpScriptError::UnboundCapture {
                                at: at.to_string(),
                                name,
                            });
                        }
                        Piece::Capture(_) => {}
                        Piece::Continuation => mints = true,
                    }
                }
            }
            Ok(mints)
        }
    }
}

fn parse(text: &str, at: &str) -> Result<Vec<Piece>, HttpScriptError> {
    template::parse(text)
        .map_err(|text| HttpScriptError::UnknownPlaceholder { at: at.to_string(), text })
}

fn bound(bindings: &HttpBindings, name: &str, at: &str) -> Result<(), HttpScriptError> {
    match bindings.get(name) {
        Some(_) => Ok(()),
        None => Err(HttpScriptError::UnknownBind { at: at.to_string(), name: name.to_string() }),
    }
}

/// A status a final response can carry (RFC 9110 §15: 1xx is interim).
pub(super) fn final_status(status: u16) -> bool {
    (200..=599).contains(&status)
}

/// A header name is a token (RFC 9110 §5.1).
pub(super) fn header_name(name: &str) -> Result<(), String> {
    let tchar = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
    if name.is_empty() || !name.bytes().all(tchar) {
        return Err("the name is not a token".to_string());
    }
    Ok(())
}

/// A header value a server sends: visible ASCII, space and tab (RFC 9110
/// §5.5; obs-text is not sent).
pub(super) fn header_value(value: &str) -> Result<(), String> {
    match value.bytes().find(|&b| !(b == b'\t' || (0x20..0x7f).contains(&b))) {
        Some(b) => Err(format!("byte 0x{b:02x} cannot be sent in a value")),
        None => Ok(()),
    }
}
