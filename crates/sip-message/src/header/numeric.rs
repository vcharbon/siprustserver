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

/// A `1*DIGIT` header value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct NumericHeader<K: NumericKind> {
    value: u32,
    kind: PhantomData<fn() -> K>,
}

impl<K: NumericKind> NumericHeader<K> {
    pub fn new(value: u32) -> Self {
        Self { value, kind: PhantomData }
    }

    pub fn value(&self) -> u32 {
        self.value
    }

    /// One less — `None` at zero, where a hop must stop rather than wrap
    /// (RFC 3261 §16.6 step 3).
    pub fn decremented(&self) -> Option<Self> {
        self.value.checked_sub(1).map(Self::new)
    }

    /// One more, saturating at the field's range.
    pub fn incremented(&self) -> Self {
        Self::new(self.value.saturating_add(1))
    }
}

impl<K: NumericKind> HeaderValue for NumericHeader<K> {
    fn header_name() -> HeaderName {
        K::name()
    }

    fn folding() -> Folding {
        K::FOLDING
    }

    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        parse_u32(raw.as_str(), K::name().as_wire_str()).map(Self::new)
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
    use crate::header::MaxForwards;

    #[test]
    fn a_hop_count_decrements_to_zero_and_stops() {
        let mf = MaxForwards::parse(&SipStr::owned("70")).unwrap();
        assert_eq!(mf.decremented().map(|m| m.value()), Some(69));
        assert_eq!(MaxForwards::new(0).decremented(), None);
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
