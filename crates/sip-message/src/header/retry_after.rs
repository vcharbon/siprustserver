//! The delta-seconds a Retry-After states (RFC 3261 §20.33), for a reader
//! that tells a refusal asking for a retry later from a bare one.

use crate::sip_str::SipStr;

/// The delta-seconds of the first Retry-After line, before its comment and
/// parameters: `None` without a line, `Some(0)` when its value does not read
/// as a number.
pub fn retry_after_seconds(lines: impl IntoIterator<Item = SipStr>) -> Option<u32> {
    let line = lines.into_iter().next()?;
    let digits: String =
        line.as_str().trim_start().chars().take_while(char::is_ascii_digit).collect();
    Some(digits.parse().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(lines: &[&str]) -> Option<u32> {
        retry_after_seconds(lines.iter().map(|l| SipStr::owned(l)))
    }

    #[test]
    fn the_first_line_s_delta_seconds_is_read() {
        assert_eq!(read(&["18"]), Some(18));
        assert_eq!(read(&[" 5 (busy);duration=60", "9"]), Some(5));
        assert_eq!(read(&["soon"]), Some(0));
        assert_eq!(read(&[]), None);
    }
}
