//! The delta-seconds a Retry-After states (RFC 3261 §20.33), for a reader
//! that tells a refusal asking for a retry later from a bare one.

use crate::sip_str::SipStr;

use super::value::HeaderValue;
use super::RetryAfter;

/// The delta-seconds of the first Retry-After line, read by the one
/// Retry-After reader: `None` without a line or when the line does not read,
/// at most `u32::MAX` for a count past it.
pub fn retry_after_seconds(lines: impl IntoIterator<Item = SipStr>) -> Option<u32> {
    let line = lines.into_iter().next()?;
    let read = RetryAfter::parse(&line).ok()?;
    let seconds = read
        .token()
        .bytes()
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
        assert_eq!(read(&["5 (busy);duration=60", "9"]), Some(5));
        assert_eq!(read(&["99999999999"]), Some(u32::MAX));
        assert_eq!(read(&[]), None);
    }

    #[test]
    fn a_line_that_does_not_read_states_no_delay() {
        assert_eq!(read(&["soon"]), None);
        assert_eq!(read(&["5 soon"]), None);
    }
}
