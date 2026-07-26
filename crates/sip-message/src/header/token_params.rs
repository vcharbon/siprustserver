//! [`TokenParamsHeader`] — the leading-token-plus-parameters family (Event,
//! Subscription-State, Content-Type, Reason, Retry-After, Session-Expires).

use std::marker::PhantomData;

use crate::error::SipParseError;
use crate::sip_str::SipStr;

use super::kind::TokenParamsKind;
use super::name::HeaderName;
use super::params::{parse_semicolon_params, ParamValue, Params, HEADER_PARAMS};
use super::scan::index_of;
use super::value::{Folding, HeaderValue};
use super::wire::Wire;

/// A token followed by `;`-parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenParamsHeader<K: TokenParamsKind> {
    token: SipStr,
    params: Params,
    kind: PhantomData<fn() -> K>,
}

impl<K: TokenParamsKind> TokenParamsHeader<K> {
    pub fn new(token: impl Into<SipStr>) -> Self {
        Self { token: token.into(), params: Params::new(), kind: PhantomData }
    }

    /// The leading token — the media type, the event package, the state.
    pub fn token(&self) -> &str {
        self.token.as_str()
    }

    pub fn is(&self, token: &str) -> bool {
        self.token.eq_ignore_ascii_case(token)
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    pub fn param(&self, name: &str) -> Option<&ParamValue> {
        self.params.get(name)
    }

    pub fn with_param(mut self, name: impl Into<SipStr>, value: ParamValue) -> Self {
        self.params.set(name, value);
        self
    }

    pub fn without_param(mut self, name: &str) -> Self {
        self.params.remove(name);
        self
    }
}

impl<K: TokenParamsKind> HeaderValue for TokenParamsHeader<K> {
    fn header_name() -> HeaderName {
        K::name()
    }

    fn folding() -> Folding {
        K::FOLDING
    }

    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        let value = raw.trimmed();
        let semi = index_of(value.as_bytes(), b';', 0).unwrap_or(value.len());
        let token = super::scan::sub_trimmed(&value, 0, semi);
        if token.is_empty() {
            return Err(SipParseError::new(format!(
                "{} has no leading token: {:?}",
                K::name(),
                value.as_str()
            )));
        }
        let (params, _) = parse_semicolon_params(&value, semi, &HEADER_PARAMS);
        Ok(Self { token, params, kind: PhantomData })
    }

    fn render(&self, out: &mut Wire) {
        out.str(self.token.as_str());
        self.params.render(out);
    }
}

impl<K: TokenParamsKind> std::fmt::Display for TokenParamsHeader<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_wire())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{MediaType, SubscriptionState};

    #[test]
    fn a_media_type_reads_its_token_and_parameters() {
        let ct = MediaType::parse(&SipStr::owned("multipart/mixed;boundary=abc")).unwrap();
        assert!(ct.is("MULTIPART/MIXED"));
        assert_eq!(ct.param("boundary").and_then(ParamValue::as_str), Some("abc"));
    }

    #[test]
    fn a_subscription_state_round_trips_its_parameters() {
        let raw = "terminated;reason=noresource;retry-after=30";
        let s = SubscriptionState::parse(&SipStr::owned(raw)).unwrap();
        assert_eq!(s.token(), "terminated");
        assert_eq!(s.to_wire(), raw);
    }

    #[test]
    fn an_empty_value_is_rejected() {
        assert!(MediaType::parse(&SipStr::owned("  ")).is_err());
    }
}
