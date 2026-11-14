//! A display name's mask: every letter, in any script or encoding, becomes
//! `x`/`X`, every digit run maps through the number mapping or is zeroed, and
//! the name keeps its byte length.

use crate::rewrite::DigitSubs;

/// Fully anonymize a display name, given as its wire bytes: basic-ASCII
/// letters → `x`/`X`, every byte of a non-ASCII character → `x` whatever its
/// script, class or encoding (a letter, an emoji, a CESU-8 surrogate, a
/// Latin-1 byte), every digit run neutralized via [`map_display_digit_run`],
/// and ASCII punctuation and spaces (the quoted-string syntax among them)
/// kept. The output is ASCII of the input's byte length, so the datagram keeps
/// its layout.
pub(super) fn anonymize_display(bytes: &[u8], digit_subs: &DigitSubs) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let run = std::str::from_utf8(&bytes[start..i]).expect("ASCII digits");
            out.extend_from_slice(map_display_digit_run(run, digit_subs).as_bytes());
        } else {
            out.push(match bytes[i] {
                b'a'..=b'z' | 0x80..=0xFF => b'x',
                b'A'..=b'Z' => b'X',
                other => other,
            });
            i += 1;
        }
    }
    out
}

/// Map one all-digit run found inside a display name: every embedded
/// DISCOVERED number maps to its fake (the same mapping as the URI
/// user-parts) and every other digit is zeroed, so the run lands in the
/// reserved space — a run holding a fake is that fake behind zeroed junk, any
/// other run all `0`s.
fn map_display_digit_run(run: &str, digit_subs: &DigitSubs) -> String {
    String::from_utf8(digit_subs.apply_zeroing(run.as_bytes()))
        .expect("digit → fake/`0` substitutions are ASCII-only")
}
