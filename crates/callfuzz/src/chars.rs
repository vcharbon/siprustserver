//! The "all chars" values: each one holds every character its RFC 3261 §25.1
//! grammar slot allows, in a drawn order, plus drawn UTF-8 where the slot
//! takes UTF-8. The classes are sip-message's ([`char_class`]).

use sip_message::char_class;

use crate::Rng;

/// Drawn non-ASCII characters a value carries where UTF-8 is legal, at most.
const UTF8_MAX: u64 = 24;

/// The ASCII bytes `class` admits, as characters.
fn ascii_of(class: fn(u8) -> bool) -> Vec<char> {
    (0u8..0x80).filter(|&b| class(b)).map(char::from).collect()
}

/// Every `token` character: a header name, a parameter name.
pub fn token(rng: &mut Rng) -> String {
    shuffled(rng, ascii_of(char_class::is_token_char))
}

/// Every character a SIP URI `user` takes unescaped, and two drawn escapes.
pub fn user(rng: &mut Rng) -> String {
    let chars = shuffled(rng, ascii_of(char_class::is_user_char));
    format!("{chars}{}{}", escape(rng), escape(rng))
}

/// Every `paramchar` character unescaped, and a drawn escape.
pub fn param(rng: &mut Rng) -> String {
    format!("{}{}", shuffled(rng, ascii_of(char_class::is_paramchar)), escape(rng))
}

/// Every `TEXT-UTF8char` of ASCII with two spaces and a tab among them, never
/// first, then drawn UTF-8: an extension header's value.
pub fn text(rng: &mut Rng) -> String {
    let mut chars: Vec<char> =
        shuffled(rng, (0x21u8..=0x7e).map(char::from).collect()).chars().collect();
    for ws in [' ', ' ', '\t'] {
        let at = 1 + rng.below(chars.len() as u64) as usize;
        chars.insert(at, ws);
    }
    format!("{}{}", chars.into_iter().collect::<String>(), utf8(rng))
}

/// Every ASCII character a quoted string carries, as `qdtext` or as the
/// `quoted-pair` the writer escapes it to, NUL, CR and LF aside; then drawn
/// UTF-8: a display name, written raw.
pub fn quoted(rng: &mut Rng) -> String {
    let chars = (0x01u8..=0x7f).filter(|b| !matches!(b, b'\r' | b'\n')).map(char::from).collect();
    format!("{}{}", shuffled(rng, chars), utf8(rng))
}

/// One to [`UTF8_MAX`] drawn scalar values past Latin-1's controls.
fn utf8(rng: &mut Rng) -> String {
    const SURROGATES: std::ops::Range<u32> = 0xd800..0xe000;
    let span = 0x11_0000 - 0xa0 - SURROGATES.len() as u64;
    (0..1 + rng.below(UTF8_MAX))
        .map(|_| {
            let cp = 0xa0 + rng.below(span) as u32;
            let cp = if cp >= SURROGATES.start { cp + SURROGATES.len() as u32 } else { cp };
            char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER)
        })
        .collect()
}

/// `%` and two drawn hex digits: any octet.
fn escape(rng: &mut Rng) -> String {
    format!("%{:02X}", rng.below(256))
}

/// `chars` in a drawn order (Fisher-Yates).
fn shuffled(rng: &mut Rng, mut chars: Vec<char>) -> String {
    for i in (1..chars.len()).rev() {
        chars.swap(i, rng.below(i as u64 + 1) as usize);
    }
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every ASCII byte `class` admits occurs in `value`.
    fn covers(value: &str, class: fn(u8) -> bool) -> bool {
        (0u8..0x80).filter(|&b| class(b)).all(|b| value.contains(char::from(b)))
    }

    #[test]
    fn each_value_is_a_run_of_its_class_and_holds_all_of_it() {
        let mut rng = Rng::new(3);
        for _ in 0..50 {
            let t = token(&mut rng);
            assert!(char_class::is_run_of(&t, char_class::is_token_char, false), "{t}");
            assert!(covers(&t, char_class::is_token_char), "{t}");
            let u = user(&mut rng);
            assert!(char_class::is_run_of(&u, char_class::is_user_char, true), "{u}");
            assert!(covers(&u, char_class::is_user_char), "{u}");
            let p = param(&mut rng);
            assert!(char_class::is_run_of(&p, char_class::is_paramchar, true), "{p}");
            assert!(covers(&p, char_class::is_paramchar), "{p}");
            let x = text(&mut rng);
            assert!(covers(&x, |b| (0x21..=0x7e).contains(&b)), "{x}");
            assert!(x.contains(' ') && x.contains('\t') && !x.starts_with([' ', '\t']), "{x:?}");
            assert!(!x.is_ascii(), "{x}");
            let q = quoted(&mut rng);
            assert!(covers(&q, |b| b != 0 && b != b'\r' && b != b'\n'), "{q:?}");
        }
    }

    #[test]
    fn no_value_holds_nul_cr_or_lf() {
        let mut rng = Rng::new(4);
        for _ in 0..200 {
            for v in [token(&mut rng), user(&mut rng), param(&mut rng), text(&mut rng)] {
                assert!(!v.contains(['\0', '\r', '\n']), "{v:?}");
            }
            assert!(!quoted(&mut rng).contains(['\0', '\r', '\n']));
        }
    }
}
