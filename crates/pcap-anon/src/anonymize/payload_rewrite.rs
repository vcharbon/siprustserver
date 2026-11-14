//! The encoded payloads the number pass rewrites decoded: a PASSporT payload
//! segment ([`crate::passport`]) and a hex User-to-User payload
//! ([`crate::uui`]). Each is decoded, passed through the digit mapping and
//! re-encoded at its width, one more same-width substitution of the whole
//! payload; a hex payload that decodes to no text is redacted to zeros
//! instead. Base64 text is no payload of this kind: it is read where it
//! stands ([`crate::encoded`]).

use crate::rewrite::DigitSubs;
use crate::{passport, uui};

/// The substitutions of every payload the mapping changes, and the count of
/// hex payloads redacted.
pub(super) struct Rewritten {
    pub(super) pairs: Vec<(Vec<u8>, Vec<u8>)>,
    pub(super) uui_redacted: usize,
}

/// The substitutions of `passports` and hex `uuis`, their decoded bytes
/// passed through `subs`.
pub(super) fn rewritten(passports: &[String], uuis: &[String], subs: &DigitSubs) -> Rewritten {
    let digits = |bytes: &[u8]| subs.apply(bytes, None);
    let mut uui_redacted = 0;
    let mut pairs = Vec::new();
    let mut push = |from: &String, to: String| {
        if to != *from {
            pairs.push((from.clone().into_bytes(), to.into_bytes()));
        }
    };
    for segment in passports {
        if let Some(to) = passport::rewrite(segment, digits) {
            push(segment, to);
        }
    }
    for hex in uuis {
        if uui::decode(hex).is_some_and(|b| uui::is_text(&b)) {
            if let Some(to) = uui::rewrite(hex, digits) {
                push(hex, to);
            }
        } else {
            uui_redacted += 1;
            push(hex, uui::redacted(hex));
        }
    }
    Rewritten { pairs, uui_redacted }
}
