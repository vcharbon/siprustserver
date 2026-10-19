//! A draw rendered as the identifier a SIP message carries.

/// Branch identifiers MUST start with this magic cookie (RFC 3261 §8.1.1.7).
pub const MAGIC_COOKIE: &str = "z9hG4bK";

/// RFC 3261 From/To tag — 8 base-36 chars.
pub fn tag(draw: u64) -> String {
    to_base36(draw, 8)
}

/// RFC 3261 Via branch with the mandatory magic cookie: `z9hG4bK` + 16 hex
/// chars.
pub fn branch(draw: u64) -> String {
    format!("{MAGIC_COOKIE}{draw:016x}")
}

/// The initial value of a 32-bit signed sequence counter, in `1..=10^8`, so a
/// peer cannot guess where a sequence resumes. The ceiling RFC 3261 §8.1.1.5
/// (CSeq) and RFC 3262 §7.1 (RSeq) share is `2^31 - 1`; starting two decimal
/// orders of magnitude below it leaves every increment a long call can make
/// inside the range.
pub fn sequence_number(draw: u64) -> u32 {
    (draw % 100_000_000) as u32 + 1
}

/// The last `len` (at most 16) base-36 digits of `v`.
fn to_base36(mut v: u64, len: usize) -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = [b'0'; 16];
    for slot in buf.iter_mut().rev() {
        #[expect(clippy::indexing_slicing, reason = "a remainder of 36 indexes the 36 digits")]
        let digit = ALPHABET[(v % 36) as usize];
        *slot = digit;
        v /= 36;
    }
    buf.iter().skip(buf.len().saturating_sub(len)).map(|&b| char::from(b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_is_eight_base36_chars() {
        for draw in [0, 1, 35, 36, u64::MAX] {
            let t = tag(draw);
            assert_eq!(t.len(), 8);
            assert!(t.bytes().all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()), "{t}");
        }
    }

    #[test]
    fn branch_carries_the_cookie_and_the_draw() {
        assert_eq!(branch(0xab), "z9hG4bK00000000000000ab");
    }

    #[test]
    fn a_sequence_start_stays_in_its_range() {
        assert_eq!(sequence_number(0), 1);
        assert_eq!(sequence_number(99_999_999), 100_000_000);
        assert_eq!(sequence_number(u64::MAX), (u64::MAX % 100_000_000) as u32 + 1);
    }
}
