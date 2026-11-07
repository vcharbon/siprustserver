//! The delta-seconds a Retry-After states (RFC 3261 §20.33), for a reader
//! that tells a refusal asking for a retry later from a bare one.

use crate::sip_str::SipStr;

/// The delta-seconds of the first Retry-After line, before its comment and
/// parameters: `None` without a line, `Some(0)` when its value does not read
/// as a number, at most `u32::MAX` for one past it.
pub fn retry_after_seconds(lines: impl IntoIterator<Item = SipStr>) -> Option<u32> {
    let line = lines.into_iter().next()?;
    let digits = line.as_str().trim_start().bytes().take_while(u8::is_ascii_digit);
    let seconds = digits
        .fold(0u64, |n, d| n.saturating_mul(10).saturating_add(u64::from(d.saturating_sub(b'0'))));
    Some(u32::try_from(seconds).unwrap_or(u32::MAX))
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
        assert_eq!(read(&["99999999999"]), Some(u32::MAX));
        assert_eq!(read(&[]), None);
    }
}
