//! Numbers not written as one digit run: with visual separators (RFC 3966
//! §5.1.1: `-`, `.`, `(` and `)` between the digits) or with escaped digits
//! (RFC 3261 §19.1.2: `%36` for `6`). A spelling maps to the fake of the
//! number it spells, each separator and each escape where it stood — an
//! escaped digit stays escaped — so digits no key spells contiguously are
//! mapped too.

use crate::rewrite::DigitSubs;

/// One written character: its decoded byte and whether it is escaped.
fn decoded(spelling: &[u8]) -> Vec<(u8, bool)> {
    let mut out = Vec::with_capacity(spelling.len());
    let mut i = 0;
    while i < spelling.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        let escape = (spelling[i] == b'%')
            .then(|| Some(hex(*spelling.get(i + 1)?)? * 16 + hex(*spelling.get(i + 2)?)?))
            .flatten();
        match escape {
            Some(byte) => {
                out.push((byte as u8, true));
                i += 3;
            }
            None => {
                out.push((spelling[i], false));
                i += 1;
            }
        }
    }
    out
}

/// `(spelling, fake spelling)` for every distinct spelling `digits` maps.
pub(super) fn pairs(spellings: &[String], digits: &DigitSubs) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for spelling in spellings {
        let chars = decoded(spelling.as_bytes());
        let bare: Vec<u8> = chars.iter().map(|(b, _)| *b).filter(u8::is_ascii_digit).collect();
        let mapped = digits.apply(&bare, None);
        if mapped.len() != bare.len() || mapped == bare {
            continue;
        }
        let (mut fake, mut rebuilt, mut at) = (mapped.into_iter(), Vec::new(), 0);
        for (byte, escaped) in chars {
            let width = if escaped { 3 } else { 1 };
            match (byte.is_ascii_digit(), escaped) {
                (true, true) => {
                    let digit = fake.next().expect("one fake digit per digit");
                    rebuilt.extend_from_slice(format!("%{:02X}", digit).as_bytes());
                }
                (true, false) => rebuilt.push(fake.next().expect("one fake digit per digit")),
                (false, _) => rebuilt.extend_from_slice(&spelling.as_bytes()[at..at + width]),
            }
            at += width;
        }
        if !out.iter().any(|(from, _)| from == spelling.as_bytes()) {
            out.push((spelling.clone().into_bytes(), rebuilt));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_separated_spelling_takes_its_numbers_fake_where_its_digits_stood() {
        let digits = DigitSubs::new(vec![(b"61212121".to_vec(), b"00091234".to_vec())]);
        let spelling = "+28-61-21-21-21".to_string();
        assert_eq!(
            pairs(&[spelling.clone(), spelling], &digits),
            vec![(b"+28-61-21-21-21".to_vec(), b"+28-00-09-12-34".to_vec())]
        );
        assert!(pairs(&["+44-1-2".to_string()], &digits).is_empty(), "nothing maps");
    }

    #[test]
    fn an_escaped_spelling_keeps_its_escapes_around_the_fake() {
        let digits = DigitSubs::new(vec![(b"61212121".to_vec(), b"00091234".to_vec())]);
        let spelling = "%2B28%361212121".to_string();
        assert_eq!(
            pairs(&[spelling], &digits),
            vec![(b"%2B28%361212121".to_vec(), b"%2B28%300091234".to_vec())]
        );
    }
}
