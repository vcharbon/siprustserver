//! The digit runs of a datagram no fake explains: every run of a phone
//! number's width ([`PHONE_WIDTH`]) or more — of [`MIN_KEY_WIDTH`] inside a
//! dialog or transaction identifier — head and body alike, that is no protocol
//! counter named by its field ([`fields::is_counter`], an SDP `o=` sess-id or
//! sess-version), no part of an address literal (the address class reads
//! those), and none the number pass wrote, inside a name the names pass
//! wrote, or zeros only. A percent escape's hex digits belong to no run, and a
//! User-to-User payload's hex is read decoded, by the number check.

use std::collections::HashSet;

use crate::anonymize::Fakes;
use crate::identities::PHONE_WIDTH;
use crate::plan::MIN_KEY_WIDTH;
use crate::rewrite::addresses;
use crate::rewrite::fields::header_name;
use crate::rewrite::fields::{digit_runs, is_counter};
use crate::rewrite::identifiers::{is_identifier, param_name};

/// The distinct unexplained digit runs of `raw`.
pub(super) fn unexplained(raw: &[u8], fakes: &Fakes) -> HashSet<Vec<u8>> {
    let literals = addresses::literals(raw);
    let mut out = HashSet::new();
    for std::ops::Range { start: i, end } in digit_runs(raw) {
        let run = &raw[i..end];
        let mut word_start =
            raw[..i].iter().rposition(|b| !b.is_ascii_alphanumeric()).map_or(0, |p| p + 1);
        // A percent escape's two hex digits spell a byte, not the word.
        if word_start >= 1 && raw[word_start - 1] == b'%' {
            word_start = (word_start + 2).min(i);
        }
        let word_end = end + raw[end..].iter().take_while(|b| b.is_ascii_alphanumeric()).count();
        let identifier = is_identifier(raw, i, end);
        // A User-to-User payload is read decoded, by the number check.
        let uui = header_name(raw, i).as_deref() == Some("user-to-user")
            || param_name(raw, i).as_deref() == Some("user-to-user");
        let width = if identifier { MIN_KEY_WIDTH } else { PHONE_WIDTH };
        let unglued = raw[word_start..word_end]
            .strip_prefix(b"z9hG4bK")
            .unwrap_or(&raw[word_start..word_end]);
        let explained = uui
            || run.len() < width
            || run.iter().all(|b| *b == b'0')
            || literals.iter().any(|&(s, e)| s < end && i < e)
            || is_counter(raw, i, end)
            || fakes.wrote_digits(run)
            || fakes.contains(&raw[word_start..word_end])
            || fakes.contains(unglued);
        if !explained {
            out.insert(run.to_vec());
        }
    }
    out
}
