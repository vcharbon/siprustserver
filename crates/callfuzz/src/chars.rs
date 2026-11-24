//! The "all chars" values: each one holds every character its RFC 3261 §25.1
//! grammar slot allows, in a drawn order, plus drawn UTF-8 where the slot
//! takes UTF-8.

use crate::Rng;

const ALPHANUM: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
/// `token`'s characters beyond `alphanum`.
const TOKEN_MARKS: &str = "-.!%*_+`'~";
/// `mark`: `unreserved`'s characters beyond `alphanum`.
const MARK: &str = "-_.!~*'()";
/// `user-unreserved`.
const USER_UNRESERVED: &str = "&=+$,;?/";
/// `param-unreserved`.
const PARAM_UNRESERVED: &str = "[]/:&+$";

/// Drawn non-ASCII characters a value carries where UTF-8 is legal, at most.
const UTF8_MAX: u64 = 24;

/// Every `token` character: a header name, a parameter name.
pub fn token(rng: &mut Rng) -> String {
    shuffled(rng, ALPHANUM.chars().chain(TOKEN_MARKS.chars()).collect())
}

/// Every character a SIP URI `user` takes unescaped, and two drawn escapes.
pub fn user(rng: &mut Rng) -> String {
    let chars = ALPHANUM.chars().chain(MARK.chars()).chain(USER_UNRESERVED.chars()).collect();
    format!("{}{}{}", shuffled(rng, chars), escape(rng), escape(rng))
}

/// Every `paramchar` character unescaped, and a drawn escape.
pub fn param(rng: &mut Rng) -> String {
    let chars = ALPHANUM.chars().chain(MARK.chars()).chain(PARAM_UNRESERVED.chars()).collect();
    format!("{}{}", shuffled(rng, chars), escape(rng))
}

/// Every `TEXT-UTF8char` of ASCII, spaces and tabs among them, then drawn
/// UTF-8: an extension header's value.
pub fn text(rng: &mut Rng) -> String {
    let mut chars: Vec<char> = (0x21u8..=0x7e).map(char::from).collect();
    chars.extend([' ', ' ', '\t']);
    let body = shuffled(rng, chars);
    let first = body.trim_start();
    format!("{first}{}", utf8(rng))
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

    fn holds_all(value: &str, set: &str) -> bool {
        set.chars().all(|c| value.contains(c))
    }

    #[test]
    fn each_value_holds_its_whole_class() {
        let mut rng = Rng::new(3);
        for _ in 0..50 {
            let t = token(&mut rng);
            assert!(holds_all(&t, ALPHANUM) && holds_all(&t, TOKEN_MARKS), "{t}");
            let u = user(&mut rng);
            assert!(holds_all(&u, MARK) && holds_all(&u, USER_UNRESERVED), "{u}");
            let p = param(&mut rng);
            assert!(holds_all(&p, PARAM_UNRESERVED), "{p}");
            let x = text(&mut rng);
            assert!((0x21u8..=0x7e).all(|b| x.contains(char::from(b))), "{x}");
            assert!(x.chars().any(|c| !c.is_ascii()), "{x}");
            let q = quoted(&mut rng);
            assert!(q.contains('"') && q.contains('\\') && q.contains('\t'), "{q}");
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
