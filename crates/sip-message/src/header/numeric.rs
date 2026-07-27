//! [`NumericHeader`] — the single-number headers (Max-Forwards,
//! Content-Length, Expires, RSeq, Min-Expires, Min-SE).

use std::marker::PhantomData;

use crate::error::SipParseError;
use crate::sip_str::SipStr;

use super::kind::NumericKind;
use super::name::HeaderName;
use super::scan::parse_u32;
use super::value::{Folding, HeaderValue};
use super::wire::Wire;

/// A `1*DIGIT` header value, always inside the range its kind states. Every
/// constructor clamps or refuses, so a value that reached this type is one the
/// header-block parser on the far side accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct NumericHeader<K: NumericKind> {
    value: u32,
    kind: PhantomData<fn() -> K>,
}

impl<K: NumericKind> NumericHeader<K> {
    /// The nearest value the header's grammar admits. A count computed from
    /// configuration, from a body length or from arithmetic cannot promise the
    /// range, and the field's own ceiling is the honest answer for one that
    /// overshoots — where stating it verbatim would emit a header no parser,
    /// this one included, accepts.
    pub fn new(value: u32) -> Self {
        Self { value: value.clamp(K::MIN, K::MAX), kind: PhantomData }
    }

    /// The value as stated, or `None` when the grammar does not admit it — the
    /// reading a peer-supplied number needs, where refusing and clamping mean
    /// different things.
    pub fn checked(value: u32) -> Option<Self> {
        (K::MIN..=K::MAX).contains(&value).then(|| Self { value, kind: PhantomData })
    }

    pub const fn value(&self) -> u32 {
        self.value
    }

    /// One less — `None` below the field's floor, where a hop must stop rather
    /// than wrap (RFC 3261 §16.6 step 3).
    pub fn decremented(&self) -> Option<Self> {
        self.value.checked_sub(1).and_then(Self::checked)
    }

    /// One more, saturating at the field's ceiling.
    pub fn incremented(&self) -> Self {
        Self::new(self.value.saturating_add(1))
    }
}

impl NumericHeader<super::kind::MaxForwards> {
    /// The hop count a UAC states when it has no reason to state another
    /// (RFC 3261 §8.1.1.6).
    pub const DEFAULT: Self = Self { value: 70, kind: PhantomData };
}

impl<K: NumericKind> HeaderValue for NumericHeader<K> {
    fn header_name() -> HeaderName {
        K::name()
    }

    fn folding() -> Folding {
        K::FOLDING
    }

    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        let value = parse_u32(raw.as_str(), K::name().as_wire_str())?;
        Self::checked(value).ok_or_else(|| {
            SipParseError::new(format!(
                "{} value {value} outside [{}, {}]",
                K::name().as_wire_str(),
                K::MIN,
                K::MAX
            ))
        })
    }

    fn render(&self, out: &mut Wire) {
        out.num(self.value as u64);
    }
}

impl<K: NumericKind> std::fmt::Display for NumericHeader<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.value, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{ContentLength, MaxForwards, RSeq};

    #[test]
    fn a_hop_count_decrements_to_zero_and_stops() {
        let mf = MaxForwards::parse(&SipStr::owned("70")).unwrap();
        assert_eq!(mf.decremented().map(|m| m.value()), Some(69));
        assert_eq!(MaxForwards::new(0).decremented(), None);
        assert_eq!(MaxForwards::DEFAULT.value(), 70);
    }

    #[test]
    fn a_value_outside_the_headers_range_never_reaches_the_wire() {
        // The hop count our own header-block parser gates at 255.
        assert_eq!(MaxForwards::new(1000).value(), 255);
        assert_eq!(MaxForwards::checked(1000), None);
        assert!(MaxForwards::parse(&SipStr::owned("256")).is_err());
        assert_eq!(MaxForwards::parse(&SipStr::owned("255")).unwrap().value(), 255);

        // RFC 3262 §7.1: an RSeq is 1..2^31-1, so zero is not one.
        assert_eq!(RSeq::checked(0), None);
        assert_eq!(RSeq::new(0).value(), 1);
        assert!(RSeq::parse(&SipStr::owned("0")).is_err());
        assert!(RSeq::parse(&SipStr::owned("2147483648")).is_err());
        assert!(RSeq::parse(&SipStr::owned("2147483647")).is_ok());

        assert!(ContentLength::parse(&SipStr::owned("2147483648")).is_err());
        assert_eq!(ContentLength::new(u32::MAX).value(), 2147483647);
    }

    #[test]
    fn a_stated_hop_count_freezes_as_a_value_the_parser_reads_back() {
        let stated = MaxForwards::new(1000);
        let reparsed = MaxForwards::parse(&SipStr::owned(&stated.to_wire()));
        assert!(reparsed.is_ok(), "a built value must survive our own parse gate");
    }

    #[test]
    fn a_non_numeric_value_is_rejected_not_saturated() {
        assert!(MaxForwards::parse(&SipStr::owned("seventy")).is_err());
        assert!(MaxForwards::parse(&SipStr::owned("")).is_err());
        assert!(MaxForwards::parse(&SipStr::owned("99999999999")).is_err());
    }

    #[test]
    fn whitespace_around_the_number_is_tolerated() {
        assert_eq!(MaxForwards::parse(&SipStr::owned(" 12 ")).unwrap().to_wire(), "12");
    }
}
