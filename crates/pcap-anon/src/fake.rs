//! The reserved fake digit space: a capture's [`Seed`] and the keyed fake
//! of one number key. A fake is `fake_prefix` + the `9` marker +
//! `decimal(HMAC-SHA256(seed, key)) mod 10^pad`, zero-padded so the fake has
//! the KEY's width (past 36 digits the hash repeats in blocks, `hash_digits`) (`pad` = key width − prefix − marker); at a width too
//! narrow for that it is the short form `0` + `9` + hash digits, and an
//! unrecognised key keeps its leading digits in front of the reserved part
//! ([`fake_keeping`]; the forms are the plan's, `plan.rs` module doc), and
//! never the key itself: a fake that spells its key is drawn again. It is
//! a pure function of (seed, key): the same number gets the same fake at every
//! site, in any discovery order, in every capture anonymised under the same
//! seed; without the seed the mapping is one-way. Two keys of one width may
//! share a fake and a shorter fake may nest in a longer one — the caller
//! counts those, nothing here refuses. The `9` marker keeps a fake out of the
//! all-zeros family. The fakes of the identities that are not numbers —
//! addresses, host names, product strings, charging ids — are [`address`] and
//! [`names`], under the same seed.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use crate::plan::{Plan, MIN_KEY_WIDTH};

pub mod address;
pub mod names;
pub mod scope;

/// The seed the crate's committed goldens and fixture expectations are
/// anonymised under (a seed file holding it reproduces them).
pub const GOLDEN_SEED_HEX: &str =
    "0180018001800180018001800180018001800180018001800180018001800180";

/// The HMAC key the whole run hashes under: 32 bytes drawn from the OS once
/// per capture and read back from the file that keeps it (`crate::seeds`).
#[derive(Clone, PartialEq, Eq)]
pub struct Seed([u8; 32]);

impl Seed {
    /// A fresh seed from the OS entropy source.
    pub fn random() -> Result<Seed, String> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| format!("cannot draw a seed: {e}"))?;
        Ok(Seed(bytes))
    }

    /// A seed under the caller's control (fixtures, tests).
    pub fn from_bytes(bytes: [u8; 32]) -> Seed {
        Seed(bytes)
    }

    /// A seed from its kept form: exactly 64 hex digits.
    pub fn from_hex(hex: &str) -> Result<Seed, String> {
        let hex = hex.trim();
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("a seed is 64 hex digits, this one has {} characters", hex.len()));
        }
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("validated hex pair");
        }
        Ok(Seed(bytes))
    }

    /// The form a seed file keeps, which `from_hex` accepts back.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Redacted: a seed printed in a log or a panic would invert the mapping.
impl std::fmt::Debug for Seed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Seed(..)")
    }
}

/// The fake of one key (an NSN, short-code or private digit string the plan
/// classified), wholly in the reserved space.
pub fn fake_for(plan: &Plan, seed: &Seed, key: &str) -> String {
    fake_keeping(plan, seed, key, 0)
}

/// The fake of one key whose first `kept` digits stay in clear (an
/// unrecognised key, `Plan::kept_prefix`): those digits, then the reserved
/// form of the remaining width — standard when it holds `fake_prefix` +
/// marker + a hash digit, short (`09` + hash digits) below that. The hash is
/// of the WHOLE key, so the kept digits never decide the fake alone.
pub fn fake_keeping(plan: &Plan, seed: &Seed, key: &str, kept: usize) -> String {
    let width = key.len() - kept;
    assert!(width >= MIN_KEY_WIDTH, "a key is at least {MIN_KEY_WIDTH} digits past its kept part");
    let (head, pad) = if width < plan.fake_prefix.len() + 2 {
        ("09".to_string(), width - 2)
    } else {
        (plan.fake_space(), width - plan.fake_prefix.len() - 1)
    };
    // A fake that spells its own key is drawn again, under a round number.
    let mut fake = format!("{}{head}{:0>pad$}", &key[..kept], hash_digits(seed, key, pad));
    let mut round = 0;
    while fake == key {
        round += 1;
        let digits = hash_digits(seed, &format!("{key}!{round}"), pad);
        fake = format!("{}{head}{digits:0>pad$}", &key[..kept]);
    }
    fake
}

/// The widest decimal reduction [`decimal_mod_pow10`] is exact for: the
/// accumulator stays under 2^128.
const BLOCK_DIGITS: usize = 36;

/// `pad` decimal digits of (seed, key), at any width. Up to [`BLOCK_DIGITS`]
/// they are the digest reduced mod `10^pad`; past that the digits are the
/// concatenated blocks of a repeated hash, block `i` being `HMAC(seed,
/// key#i)` reduced to [`BLOCK_DIGITS`] digits, truncated to `pad`. Both forms
/// are pure in (seed, key) and the narrow one is untouched, so a key the plan
/// admits keeps the fake it had.
fn hash_digits(seed: &Seed, key: &str, pad: usize) -> String {
    if pad <= BLOCK_DIGITS {
        return decimal_mod_pow10(&digest(seed, key.as_bytes()), pad);
    }
    let mut out = String::with_capacity(pad);
    for block in 0.. {
        if out.len() >= pad {
            break;
        }
        let d = decimal_mod_pow10(&digest(seed, format!("{key}#{block}").as_bytes()), BLOCK_DIGITS);
        let padded = format!("{d:0>width$}", width = BLOCK_DIGITS);
        out.push_str(&padded[..BLOCK_DIGITS.min(pad - out.len())]);
    }
    out
}

/// `HMAC-SHA256(seed, msg)`.
pub(crate) fn digest(seed: &Seed, msg: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(&seed.0).expect("HMAC takes any key length");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

/// The 256-bit digest as a big-endian integer, reduced mod `10^pad`, as a
/// decimal string of at most `pad` digits. Callers stay within
/// [`BLOCK_DIGITS`], the width the reduction is exact for.
fn decimal_mod_pow10(digest: &[u8], pad: usize) -> String {
    if pad == 0 {
        return String::new();
    }
    assert!(pad <= BLOCK_DIGITS, "the reduction is exact only up to {BLOCK_DIGITS} digits");
    let modulus: u128 = 10u128.pow(pad as u32);
    let mut acc: u128 = 0;
    for &b in digest {
        acc = (acc * 256 + u128::from(b)) % modulus;
    }
    acc.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_round_trips_through_hex() {
        let seed = Seed::from_bytes([0xab; 32]);
        assert_eq!(seed.to_hex().len(), 64);
        assert_eq!(Seed::from_hex(&seed.to_hex()).unwrap(), seed);
        assert!(Seed::from_hex("abc").is_err());
        assert!(Seed::from_hex(&"zz".repeat(32)).is_err());
        assert_ne!(Seed::random().unwrap(), Seed::random().unwrap());
    }

    #[test]
    fn fake_is_marker_prefixed_key_wide_and_a_function_of_seed_and_key() {
        let plan = crate::testflow::plan();
        let seed = Seed::from_bytes([1; 32]);
        for key in ["41001", "22345678", "123456789012"] {
            let fake = fake_for(&plan, &seed, key);
            assert_eq!(fake.len(), key.len());
            assert!(fake.starts_with("0009"), "{fake}");
            assert!(fake.bytes().all(|b| b.is_ascii_digit()));
            assert_eq!(fake, fake_for(&plan, &seed, key), "pure in (seed, key)");
        }
        assert_ne!(
            fake_for(&plan, &seed, "22345678"),
            fake_for(&plan, &Seed::from_bytes([2; 32]), "22345678")
        );
    }

    /// A 4-digit key takes the short form; an unrecognised key keeps its
    /// leading digits and the rest is the standard form, hashed over the
    /// whole key.
    #[test]
    fn short_and_kept_forms_have_the_key_width_and_a_recognisable_shape() {
        let plan = crate::testflow::plan();
        let seed = Seed::from_bytes([1; 32]);
        let short = fake_for(&plan, &seed, "2210");
        assert_eq!(short.len(), 4);
        assert!(short.starts_with("09"), "{short}");
        assert!(plan.is_fake(&short));
        assert_eq!(fake_keeping(&plan, &seed, "2210", 0), short);

        let kept = fake_keeping(&plan, &seed, "83571234567890", 3);
        assert_eq!(kept.len(), 14);
        assert!(kept.starts_with("8350009"), "{kept}");
        assert!(plan.is_fake(&kept[3..]));
        assert_ne!(
            kept[3..],
            fake_keeping(&plan, &seed, "83571234567891", 3)[3..],
            "the hash covers the whole key"
        );
        // Kept digits shrink the reserved width down to the short form.
        assert!(fake_keeping(&plan, &seed, "286123", 1).starts_with("20009"));
        assert!(fake_keeping(&plan, &seed, "2861234", 2).starts_with("280009"));
    }

    /// A key wider than one hash block still gets a fake of its own width,
    /// all digits, pure in (seed, key) and sensitive to every digit of it.
    #[test]
    fn a_key_past_one_hash_block_is_faked_at_its_full_width() {
        let plan = crate::testflow::plan();
        let seed = Seed::from_bytes([1; 32]);
        for width in [40usize, 64, 120] {
            let key = "1".repeat(width);
            let fake = fake_for(&plan, &seed, &key);
            assert_eq!(fake.len(), width);
            assert!(fake.bytes().all(|b| b.is_ascii_digit()), "{fake}");
            assert!(fake.starts_with(&plan.fake_space()), "{fake}");
            assert_eq!(fake, fake_for(&plan, &seed, &key), "pure in (seed, key)");
            let mut other = key.clone();
            other.push('7');
            assert_ne!(fake_for(&plan, &seed, &other)[..width], fake[..width]);
        }
        // Blocks differ: the fake is not one block tiled.
        let wide = fake_for(&plan, &seed, &"2".repeat(80));
        assert_ne!(wide[4..40], wide[40..76]);
    }

    /// No key is its own fake, in the short space (100 fakes a width) or the
    /// standard one (10 at the narrowest), under any seed.
    #[test]
    fn no_key_is_its_own_fake() {
        let plan = crate::testflow::plan();
        for n in 0..64u8 {
            let seed = Seed::from_bytes([n; 32]);
            for tail in 0..100 {
                let short = format!("09{tail:02}");
                assert_ne!(fake_for(&plan, &seed, &short), short, "seed {n}");
            }
            for last in 0..10 {
                let standard = format!("0009{last}");
                assert_ne!(fake_for(&plan, &seed, &standard), standard, "seed {n}");
            }
        }
    }

    #[test]
    fn decimal_reduction_is_exact_against_a_small_modulus() {
        // 0x0102 = 258 → 258 mod 10 = 8, mod 100 = 58, mod 1000 = 258.
        assert_eq!(decimal_mod_pow10(&[1, 2], 1), "8");
        assert_eq!(decimal_mod_pow10(&[1, 2], 2), "58");
        assert_eq!(decimal_mod_pow10(&[1, 2], 3), "258");
        assert_eq!(decimal_mod_pow10(&[1, 2], 0), "");
    }
}
