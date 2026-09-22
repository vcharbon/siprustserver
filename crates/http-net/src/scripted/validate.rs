//! The checks `add` runs: a script it accepts can be served as written.

use std::collections::{BTreeSet, HashSet};

use super::error::HttpScriptError;
use super::program::{HttpBindings, HttpReply, HttpRequestMatch, HttpScript, HttpScriptStep};
use super::template::{self, Piece};

/// What `add` keeps of a checked script's open.
pub(super) struct Checked {
    /// The open's fragments, bindings resolved: the open-match rule's sets.
    pub fragments: BTreeSet<String>,
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
    let mut attributable = false;
    let mut fragments = BTreeSet::new();
    for entry in &script.open.contains {
        attributable |= template::parse(entry)
            .map(|pieces| pieces.iter().any(|p| matches!(p, Piece::Bind(_))))
            .unwrap_or(false);
        fragments.insert(template::show(entry, bindings));
    }
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
        HttpReply::Respond { headers, body, .. } => {
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
