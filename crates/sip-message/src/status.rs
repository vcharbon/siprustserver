//! Status codes as protocol facts, independent of any message.

/// The status codes this stack's B2BUA and front proxy send or match on, as
/// wire digits, ascending: the codes their per-status metric families
/// declare. Any other code appears on its first observation (the parser
/// bounds codes to 100..=699).
pub const STACK_CODES: [&str; 27] = [
    "100", "180", "183", "200", "202", "302", "400", "401", "403", "404", "405", "407", "408",
    "420", "480", "481", "483", "486", "487", "488", "491", "500", "501", "502", "503", "504",
    "603",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_codes_are_three_digit_and_ascending() {
        for w in STACK_CODES.windows(2) {
            assert!(w[0] < w[1], "{} before {}", w[0], w[1]);
        }
        assert!(STACK_CODES.iter().all(|c| c.len() == 3));
    }
}
