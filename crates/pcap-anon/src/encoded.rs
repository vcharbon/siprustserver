//! Text carried base64-encoded. One rule: a base64 run that decodes to text
//! is that text standing in clear at the same place. The harvest reads it
//! there, the rewrite writes into it what it writes into clear text there and
//! re-encodes it in its alphabet with its padding, and the leak check reads
//! it there ([`sites`]). A text that is itself base64 is read one level deep.
//!
//! A run is a maximal stretch of base64 characters — letters, digits, `+` `/`
//! (the standard alphabet) and `-` `_` (the URL-safe one) — with up to two
//! `=` behind it. It decodes to text when it is at least [`MIN_RUN`]
//! characters long, holds a letter (digits alone are digits as written), is
//! not hex digits alone (an id, a hex payload), uses one alphabet, decodes
//! canonically, its padding complete or absent, and its bytes are UTF-8
//! without a control character: a line break would move the text to another
//! place. A run that does not decode whole is read by its longest stretches
//! between `+` `/` `-` `_` that do, so a neighbour glued by one of them
//! (`v1/…`, `id-…`, `…-1`) hides nothing; a run of more than [`MAX_PIECES`]
//! such stretches is read whole or not at all.
//!
//! Random bytes decode to text by accident, about 0.45 per byte, so a run is
//! never the decoded reading's alone: the passes read it as written too
//! ([`rewrite`]). Text is beyond accident ([`Text::sure`]) at [`SURE_TEXT`]
//! bytes, or holding the digits of a phone number, or where its place states
//! base64 ([`Site::sure`]): the harvest and the leak checks that judge by
//! shape read that text decoded. A shorter text adds no key and is judged as
//! written; the rewrite maps in it the keys the rest of the capture gives,
//! and the leak checks search it for what the raw document held.

use std::ops::Range;

use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use base64::Engine as _;

use crate::identities::PHONE_WIDTH;

mod sites;

pub use sites::{decoded, read, read_sure, rewrite, sites, Context, Decoded, Site};

/// The shortest run read, its padding counted: base64 of [`PHONE_WIDTH`]
/// bytes, the narrowest number a header value carries on its own. A shorter
/// run decodes to text by accident too often to be read.
pub const MIN_RUN: usize = PHONE_WIDTH.div_ceil(3) * 4;

/// The byte width from which text is beyond accident: random bytes spell
/// that much UTF-8 without a control character less than once in a million
/// runs.
pub const SURE_TEXT: usize = 20;

/// The most stretches between `+` `/` `-` `_` a run is read by.
pub const MAX_PIECES: usize = 16;

/// The text one run decodes to, and how the run writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text {
    /// The decoded text.
    pub text: Vec<u8>,
    url_safe: bool,
    pad: usize,
}

impl Text {
    /// The text `run` decodes to (module doc), if it does.
    pub fn of(run: &str) -> Option<Text> {
        let unpadded = run.trim_end_matches('=');
        let pad = run.len() - unpadded.len();
        let has = |set: &[u8]| unpadded.bytes().any(|b| set.contains(&b));
        let (standard, url_safe) = (has(b"+/"), has(b"-_"));
        let complete = pad == 0 || run.len().is_multiple_of(4);
        let letter = unpadded.bytes().any(|b| b.is_ascii_alphabetic());
        let hex = unpadded.bytes().all(|b| b.is_ascii_hexdigit());
        if run.len() < MIN_RUN || pad > 2 || !complete || !letter || hex || (standard && url_safe) {
            return None;
        }
        let engine = if url_safe { URL_SAFE_NO_PAD } else { STANDARD_NO_PAD };
        let text = engine.decode(unpadded).ok()?;
        let read = std::str::from_utf8(&text).ok()?;
        (!read.is_empty() && !read.chars().any(char::is_control)).then_some(Text {
            text,
            url_safe,
            pad,
        })
    }

    /// `text` written as the run was: in its alphabet, with its padding.
    pub fn encode(&self, text: &[u8]) -> String {
        let engine = if self.url_safe { URL_SAFE_NO_PAD } else { STANDARD_NO_PAD };
        let mut out = engine.encode(text);
        out.extend(std::iter::repeat_n('=', self.pad));
        out
    }

    /// Whether the text is beyond accident: [`SURE_TEXT`] bytes of it, or
    /// [`PHONE_WIDTH`] digits in all, a number however it is spelt.
    pub fn sure(&self) -> bool {
        self.text.len() >= SURE_TEXT
            || self.text.iter().filter(|b| b.is_ascii_digit()).count() >= PHONE_WIDTH
    }
}

/// Every stretch of `bytes` that decodes to text (module doc), in order, none
/// overlapping, with its text.
pub fn runs(bytes: &[u8]) -> Vec<(Range<usize>, Text)> {
    let body = |b: &u8| b.is_ascii_alphanumeric() || b"+/-_".contains(b);
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !body(&bytes[i]) {
            i += 1;
            continue;
        }
        let end = i + bytes[i..].iter().take_while(|b| body(b)).count();
        let padded = end + bytes[end..].iter().take(2).take_while(|b| **b == b'=').count();
        if padded - i >= MIN_RUN {
            out.extend(
                stretches(&bytes[i..padded], end - i)
                    .into_iter()
                    .map(|(r, text)| (i + r.start..i + r.end, text)),
            );
        }
        i = padded;
    }
    out
}

/// The stretches of one maximal `run` that decode to text, its first `body`
/// bytes being its alphabet characters: the run whole, else its longest
/// stretches between two of `+` `/` `-` `_`, the padding going with the last.
fn stretches(run: &[u8], body: usize) -> Vec<(Range<usize>, Text)> {
    let read = |r: Range<usize>| {
        let run = std::str::from_utf8(&run[r.clone()]).expect("base64 characters are ASCII");
        Text::of(run).map(|text| (r, text))
    };
    if let Some(whole) = read(0..run.len()) {
        return vec![whole];
    }
    let cuts: Vec<usize> = (0..body).filter(|&i| b"+/-_".contains(&run[i])).collect();
    if cuts.is_empty() || cuts.len() >= MAX_PIECES {
        return Vec::new();
    }
    let starts = std::iter::once(0).chain(cuts.iter().map(|c| c + 1));
    let ends: Vec<usize> = cuts.iter().copied().chain(std::iter::once(run.len())).collect();
    let mut candidates: Vec<Range<usize>> = starts
        .flat_map(|s| ends.iter().map(move |&e| s..e))
        .filter(|r| r.start < r.end && r.len() >= MIN_RUN && r.len() < run.len())
        .collect();
    candidates.sort_by(|a, b| b.len().cmp(&a.len()).then(a.start.cmp(&b.start)));
    let mut found: Vec<(Range<usize>, Text)> = Vec::new();
    for candidate in candidates {
        if found
            .iter()
            .all(|(taken, _)| candidate.end <= taken.start || taken.end <= candidate.start)
        {
            found.extend(read(candidate));
        }
    }
    found.sort_by_key(|(r, _)| r.start);
    found
}

#[cfg(test)]
mod tests;
