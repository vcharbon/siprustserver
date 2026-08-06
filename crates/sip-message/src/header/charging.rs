//! [`ChargingVector`] — the RFC 7315 §5.6 charging correlation value carried on
//! `P-Charging-Vector`.
//!
//! The `icid-value` is minted ONCE, by the element that starts the leg, and
//! travels unchanged from there on: it is the key the two operators' records
//! are matched on, so a value re-minted mid-path breaks the correlation the
//! header exists for. `icid-generated-at` names the element that minted it.

use crate::error::SipParseError;
use crate::parser::custom::scanner::is_token_char;
use crate::sip_str::SipStr;

use super::name::HeaderName;
use super::params::Params;
use super::value::{Folding, HeaderValue};
use super::wire::Wire;

/// RFC 7315 §5.6 charging correlation: the identifier of the charging session
/// plus the element that generated it. Inter-operator identifiers (`orig-ioi` /
/// `term-ioi`) are not modelled — an element that receives them relays the
/// header verbatim rather than rebuilding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChargingVector {
    icid_value: SipStr,
    generated_at: Option<SipStr>,
}

impl ChargingVector {
    /// The vector identifying this charging session, generated at `host`.
    pub fn new(icid_value: impl Into<SipStr>, host: impl Into<SipStr>) -> Self {
        Self { icid_value: icid_value.into(), generated_at: Some(host.into()) }
    }

    /// The correlation identifier itself.
    pub fn icid_value(&self) -> &str {
        self.icid_value.as_str()
    }

    /// The element that generated [`icid_value`](Self::icid_value), when stated.
    pub fn icid_generated_at(&self) -> Option<&str> {
        self.generated_at.as_ref().map(SipStr::as_str)
    }
}

/// A `gen-value` reaches the wire bare when it is an RFC 3261 §25.1 `token`,
/// quoted otherwise — a quoted-string round-trips through a re-parse unchanged.
fn gen_value(out: &mut Wire, value: &str) {
    if !value.is_empty() && value.bytes().all(is_token_char) {
        out.str(value);
    } else {
        out.quoted(value);
    }
}

impl HeaderValue for ChargingVector {
    fn header_name() -> HeaderName {
        HeaderName::Other(SipStr::from_static("P-Charging-Vector"))
    }

    fn folding() -> Folding {
        Folding::Single
    }

    /// A value carrying no `icid-value` is not a charging vector: RFC 7315 §5.6
    /// makes the identifier the header's only mandatory parameter.
    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        let params = Params::parse_list(raw);
        let icid_value = params
            .value("icid-value")
            .ok_or_else(|| SipParseError::new("P-Charging-Vector carries no icid-value"))?;
        Ok(Self {
            icid_value: SipStr::owned(icid_value),
            generated_at: params.value("icid-generated-at").map(SipStr::owned),
        })
    }

    fn render(&self, out: &mut Wire) {
        out.str("icid-value=");
        gen_value(out, self.icid_value.as_str());
        if let Some(host) = &self.generated_at {
            out.str(";icid-generated-at=");
            gen_value(out, host.as_str());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7315 §5.6 layout: the identifier first, then the element that minted
    /// it, `;`-separated with no leading separator.
    #[test]
    fn a_minted_vector_renders_the_rfc_7315_layout() {
        let cv = ChargingVector::new("cafe0001-b-1", "10.0.0.7");
        assert_eq!(cv.to_wire(), "icid-value=cafe0001-b-1;icid-generated-at=10.0.0.7");
    }

    /// A value that is not a `token` is quoted, so it survives a re-parse.
    #[test]
    fn a_non_token_identifier_is_quoted_and_reads_back() {
        let cv = ChargingVector::new("has space", "host");
        assert_eq!(cv.to_wire(), "icid-value=\"has space\";icid-generated-at=host");
        let reread = ChargingVector::parse(&SipStr::owned(&cv.to_wire())).unwrap();
        assert_eq!(reread, cv);
    }

    /// A received vector carrying inter-operator identifiers still answers for
    /// its correlation key — the extra parameters are simply not modelled.
    #[test]
    fn a_received_vector_reads_its_identifier_past_unmodelled_parameters() {
        let raw = SipStr::owned("icid-value=abc123;icid-generated-at=p.example;orig-ioi=example");
        let cv = ChargingVector::parse(&raw).unwrap();
        assert_eq!(cv.icid_value(), "abc123");
        assert_eq!(cv.icid_generated_at(), Some("p.example"));
    }

    #[test]
    fn a_value_without_an_identifier_is_not_a_charging_vector() {
        assert!(ChargingVector::parse(&SipStr::owned("orig-ioi=example")).is_err());
    }
}
