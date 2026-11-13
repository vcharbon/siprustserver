//! The boundary of a body the stack frames itself (RFC 2046 §5.1.1): derived
//! from the parts, so the same parts always frame the same way, of one fixed
//! length, and held by no part.

use lookup_map::LookupSet;
use memchr::memmem;

use super::{fields, MultipartPart};

/// What every derived boundary starts with. No proper prefix of it is a
/// suffix of it, so its occurrences in a text never overlap.
const PREFIX: &str = "boundary-";

/// The lowercase hex digits after [`PREFIX`]: one `u64`.
const DIGITS: usize = 16;

/// A boundary no part holds, in its type, a head line or its payload, and the
/// same for the same parts: [`PREFIX`] and the number of one `u64` as 16
/// lowercase hex digits, 25 of the 70 `bchars` RFC 2046 §5.1.1 allows. The
/// number starts at a hash of the parts and steps past every number a part
/// holds after the prefix, so the work is two passes over the parts and one
/// step per number held, whatever the parts hold.
pub(super) fn derive_boundary(parts: &[MultipartPart]) -> String {
    boundary_from(start(parts), parts)
}

/// [`derive_boundary`], the number the steps start at stated.
fn boundary_from(start: u64, parts: &[MultipartPart]) -> String {
    let number = first_free(start, &held_numbers(parts));
    format!("{PREFIX}{number:016x}")
}

/// The first of `start`, `start + 1`, … (wrapping) that `held` lacks.
/// `held.len()` steps reach it: that many held numbers in a row are all of
/// them, so the next one is free.
fn first_free(start: u64, held: &LookupSet<u64>) -> u64 {
    let mut number = start;
    for _ in 0..held.len() {
        if !held.contains(&number) {
            break;
        }
        number = number.wrapping_add(1);
    }
    number
}

/// Every number a field of `parts` holds as a derived boundary spells it:
/// [`DIGITS`] lowercase hex digits right after [`PREFIX`].
fn held_numbers(parts: &[MultipartPart]) -> LookupSet<u64> {
    let prefix = memmem::Finder::new(PREFIX);
    parts
        .iter()
        .flat_map(fields)
        .flat_map(|field| {
            prefix.find_iter(field).filter_map(move |at| number_at(field, at + PREFIX.len()))
        })
        .collect()
}

/// The number [`DIGITS`] lowercase hex digits spell at `at` of `field`.
fn number_at(field: &[u8], at: usize) -> Option<u64> {
    let digits = field.get(at..at.checked_add(DIGITS)?)?;
    digits.iter().try_fold(0u64, |number, &digit| Some((number << 4) | u64::from(nibble(digit)?)))
}

/// The value of one lowercase hex digit.
fn nibble(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A hash (FNV-1a, 64 bits) of everything `parts` are framed from: each
/// part's count of head lines, then its type, each head line's name and
/// value and its payload, each after its length. A byte added to a field,
/// or moved from one to the next, is another input.
fn start(parts: &[MultipartPart]) -> u64 {
    parts.iter().fold(FNV_OFFSET, |hash, part| {
        let hash = hashed(hash, &part.headers.len().to_le_bytes());
        fields(part)
            .fold(hash, |hash, field| hashed(hashed(hash, &field.len().to_le_bytes()), field))
    })
}

/// `hash` after `bytes`.
fn hashed(hash: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(hash, |hash, &byte| (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::super::framing::frames_cleanly;
    use super::super::{entity_parts, frame, holds, is_boundary};
    use super::*;

    /// The boundary the derivation spells for `number`.
    fn spelled(number: u64) -> String {
        format!("boundary-{number:016x}")
    }

    fn text(payload: impl Into<Vec<u8>>) -> MultipartPart {
        MultipartPart::new("text/plain", payload.into())
    }

    #[test]
    fn a_boundary_is_25_bchars_spelling_the_start_no_part_holds() {
        let boundary = boundary_from(0x1234, &[text("hello")]);
        assert_eq!(boundary, "boundary-0000000000001234");
        assert!(is_boundary(&boundary));
        assert_eq!(derive_boundary(&[text("hello")]).len(), 25);
    }

    /// A part holding the start's boundary in any field it is framed from
    /// (its type, a head line's name or value, its payload) frames under the
    /// next number, and reads back.
    #[test]
    fn a_number_a_part_holds_is_stepped_past_in_every_field() {
        let start = 0x00c0_ffee_0000_00ff;
        let held = spelled(start);
        let holders = [
            MultipartPart::new(format!("text/plain;note={held}"), b"x".to_vec()),
            text("x").with_header(format!("--{held}"), "v"),
            text("x").with_header("X-Note", format!("see {held}--")),
            text(format!("line\r\n--{held}\r\nline")),
            text(format!("boundary-{held}")),
        ];
        for holder in holders {
            let parts = [text("first"), holder];
            let boundary = boundary_from(start, &parts);
            assert_eq!(boundary, spelled(start + 1), "{parts:?}");
            let body = frame(&boundary, &parts);
            assert!(frames_cleanly(&body, &boundary), "{}", String::from_utf8_lossy(&body));
            let content_type = format!("multipart/mixed;boundary={boundary}");
            assert_eq!(entity_parts(Some(&content_type), &[], &body), parts);
        }
    }

    /// Held numbers in a row, in any order and over several parts, are all
    /// stepped past, across the wrap from the largest number to zero.
    #[test]
    fn a_run_of_held_numbers_is_stepped_past_across_the_wrap() {
        let start = u64::MAX - 2;
        let run = |numbers: &[u64]| numbers.iter().map(|n| spelled(*n)).collect::<Vec<_>>();
        let parts = [
            text(run(&[1, u64::MAX, 7]).join(" ")),
            text("x").with_header("X-Note", run(&[0, u64::MAX - 2, u64::MAX - 1]).concat()),
        ];
        let boundary = boundary_from(start, &parts);
        assert_eq!(boundary, spelled(2));
        assert!(parts.iter().all(|part| !holds(part, &boundary)));
    }

    /// What is not the start's boundary byte for byte leaves it free: digits
    /// in upper case, fewer than sixteen of them, another prefix.
    #[test]
    fn only_the_boundary_as_spelled_counts_as_held() {
        let start = 0xabcd_ef01_2345_6789;
        let held = spelled(start);
        let parts = [
            text(held.to_uppercase()),
            text("boundary-ABCDEF0123456789"),
            text("x").with_header("X-Note", held.get(..held.len() - 1).unwrap()),
            text(held.replacen("boundary-", "boundary_", 1)),
        ];
        assert_eq!(boundary_from(start, &parts), held);
    }

    /// The start covers every field: parts that differ in a head line's name
    /// or value, in which field a byte sits, or in which part a line sits,
    /// derive different boundaries; equal parts derive the same.
    #[test]
    fn the_derived_boundary_changes_with_any_field() {
        let base = [text("one").with_header("X-Note", "ab"), text("two")];
        let variants = [
            vec![text("one").with_header("X-Note", "ac"), text("two")],
            vec![text("one").with_header("X-Nota", "ab"), text("two")],
            vec![text("one").with_header("X-Notea", "b"), text("two")],
            vec![text("one"), text("two").with_header("X-Note", "ab")],
            vec![text("one").with_header("X-Note", "ab").with_header("X-Note", "ab"), text("two")],
            vec![text("on").with_header("X-Note", "ab"), text("etwo")],
        ];
        assert_eq!(derive_boundary(&base), derive_boundary(&base.clone()));
        for variant in variants {
            assert_ne!(derive_boundary(&base), derive_boundary(&variant), "{variant:?}");
        }
    }

    /// A text of noise and of boundaries spelled for numbers at and just
    /// after `start`, whole, cut short and in upper case.
    fn planted_text(start: u64) -> impl Strategy<Value = String> {
        let piece = prop_oneof![
            "[a-z0-9 -]{0,12}",
            Just("boundary-".to_string()),
            (0u64..6).prop_map(move |step| spelled(start.wrapping_add(step))),
            (0u64..6).prop_map(move |step| spelled(start.wrapping_add(step)).to_uppercase()),
            (0u64..6, 10usize..25).prop_map(move |(step, keep)| {
                spelled(start.wrapping_add(step)).chars().take(keep).collect()
            }),
        ];
        proptest::collection::vec(piece, 0..6).prop_map(|pieces| pieces.concat())
    }

    fn planted_part(start: u64) -> impl Strategy<Value = MultipartPart> {
        let line = (planted_text(start), planted_text(start));
        (planted_text(start), proptest::collection::vec(line, 0..3), planted_text(start)).prop_map(
            |(content_type, headers, payload)| MultipartPart {
                content_type,
                headers,
                payload: payload.into_bytes(),
            },
        )
    }

    /// A start anywhere, the wrap included, and parts holding numbers at and
    /// just after it.
    fn planted_parts() -> impl Strategy<Value = (u64, Vec<MultipartPart>)> {
        prop_oneof![any::<u64>(), (u64::MAX - 8)..=u64::MAX].prop_flat_map(|start| {
            (Just(start), proptest::collection::vec(planted_part(start), 1..4))
        })
    }

    proptest! {
        /// Whatever the parts hold, the boundary has RFC 2046 §5.1.1's form
        /// and no part holds it.
        #[test]
        fn a_boundary_is_well_formed_and_held_by_no_part((start, parts) in planted_parts()) {
            let boundary = boundary_from(start, &parts);
            prop_assert!(is_boundary(&boundary), "{boundary}");
            prop_assert!(parts.iter().all(|part| !holds(part, &boundary)), "{boundary} in {parts:?}");
        }
    }
}
