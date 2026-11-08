//! [`TokenParamsHeader`] — the leading-token-plus-parameters family (Event,
//! Subscription-State, Reason, Retry-After, Session-Expires, Replaces,
//! Refer-Sub, Content-Disposition, Accept). Each kind states what
//! leads its value ([`Leading`]) and how its parameters read.

use std::marker::PhantomData;
use std::ops::ControlFlow::{Break, Continue};

use crate::cursor::walk;
use crate::error::SipParseError;
use crate::sip_str::SipStr;

use super::cardinality::held_to_the_table;
use super::identity::is_call_id;
use super::kind::TokenParamsKind;
use super::params::{read_params, ParamReading, ParamValue, Params};
use super::scan::{byte_at, clamped, digits_end, scan_until, skip_ws, sub, token_end};
use super::single_valued::Listed;
use super::value::HeaderValue;
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

    /// Whether the leading token is `token`, compared as its grammar compares
    /// it: byte by byte where [`TokenParamsKind::EXACT`] says so (an event
    /// package, RFC 6665 §8.2.1), ignoring ASCII case otherwise.
    pub fn is(&self, token: &str) -> bool {
        if K::EXACT {
            self.token.as_str() == token
        } else {
            self.token.eq_ignore_ascii_case(token)
        }
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

/// Parse a `token *(;param)` value with NO named header type — the shape a
/// number of extension headers share (RFC 7315 §5.4 `P-Access-Network-Info`
/// among them). Yields the leading token and the parameter list, the whole
/// value read; the grammar's LWS freedom (`SWS ";" SWS`) is absorbed by the
/// parse, so two spellings of one value compare equal through the parts.
pub fn parse_token_params(raw: &SipStr) -> Result<(SipStr, Params), SipParseError> {
    read_token_params(raw, Leading::Token, ParamReading::Strict)
        .map_err(|reason| SipParseError::new(format!("{reason}: {:?}", raw.as_str())))
}

/// What leads a value of this family (RFC 3261 §25.1 unless stated).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leading {
    /// A `token`.
    Token,
    /// One of these tokens, case aside (RFC 4488 §3 `refer-sub-value`).
    OneOf(&'static [&'static str]),
    /// `callid = word [ "@" word ]` (RFC 3891 §6.1).
    CallId,
    /// `m-type SLASH m-subtype`, two tokens, kept as written: the media range
    /// of an Accept line. A Content-Type reads through its own type,
    /// [`MediaType`](super::MediaType).
    MediaType,
    /// `delta-seconds = 1*DIGIT` (RFC 4028 §4).
    DeltaSeconds,
    /// `delta-seconds [ comment ]` (§20.33 `Retry-After`).
    DeltaSecondsComment,
}

/// The leading part of `value` as `leading` reads it, and where the
/// parameters start; `Err` names what it is not.
fn read_leading(value: &SipStr, leading: Leading) -> Result<(SipStr, usize), String> {
    let bytes = value.as_bytes();
    let token = |end: usize| sub(value, 0, end);
    match leading {
        Leading::Token | Leading::OneOf(_) => {
            let end = token_end(bytes, 0);
            if end == 0 {
                return Err("no leading token".to_string());
            }
            if let Leading::OneOf(values) = leading {
                let read = clamped(value.as_str(), 0, end);
                if !values.iter().any(|v| v.eq_ignore_ascii_case(read)) {
                    return Err(format!("leading token is none of {values:?}"));
                }
            }
            Ok((token(end), end))
        }
        Leading::CallId => {
            let end = scan_until(bytes, 0, b"; \t");
            if !is_call_id(clamped(value.as_str(), 0, end)) {
                return Err("leading part is no word [\"@\" word] Call-ID".to_string());
            }
            Ok((token(end), end))
        }
        Leading::MediaType => {
            let type_end = token_end(bytes, 0);
            let slash = skip_ws(bytes, type_end);
            let sub_at = skip_ws(bytes, slash + 1);
            let end = token_end(bytes, sub_at);
            if type_end == 0 || byte_at(bytes, slash) != Some(b'/') || end == sub_at {
                return Err("leading part is no type/subtype".to_string());
            }
            Ok((token(end), end))
        }
        Leading::DeltaSeconds | Leading::DeltaSecondsComment => {
            let end = digits_end(bytes, 0);
            if end == 0 {
                return Err("leading part is no delta-seconds".to_string());
            }
            if leading == Leading::DeltaSeconds {
                return Ok((token(end), end));
            }
            let open = skip_ws(bytes, end);
            if byte_at(bytes, open) != Some(b'(') {
                return Ok((token(end), end));
            }
            let close = comment_close(bytes, open).ok_or("unterminated comment")?;
            Ok((token(end), close))
        }
    }
}

/// The position after the `)` closing the `comment` opened at byte `open`
/// (RFC 3261 §25.1): `ctext`, `quoted-pair` and nested comments.
fn comment_close(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    walk(bytes, open, |i| match byte_at(bytes, i) {
        None => Break(None),
        Some(b'(') => {
            depth += 1;
            Continue(i + 1)
        }
        Some(b')') => {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                Break(Some(i + 1))
            } else {
                Continue(i + 1)
            }
        }
        Some(b'\\') => match byte_at(bytes, i + 1) {
            Some(b) if b.is_ascii() && b != b'\r' && b != b'\n' => Continue(i + 2),
            _ => Break(None),
        },
        Some(_) => Continue(i + 1),
    })
    .ok()
    .flatten()
}

/// `leading *( SEMI generic-param )`, the whole of `raw` but its surrounding
/// LWS; [`ParamReading::UpToFirstBad`] keeps the parameters before the first
/// off its grammar and refuses only the leading part.
fn read_token_params(
    raw: &SipStr,
    leading: Leading,
    reading: ParamReading,
) -> Result<(SipStr, Params), String> {
    let value = raw.trimmed();
    let (token, params_at) = read_leading(&value, leading)?;
    let (params, _) = read_params(&value, params_at, reading, None).map_err(|e| e.reason)?;
    Ok((token, params))
}

impl<K: TokenParamsKind> HeaderValue for TokenParamsHeader<K> {
    type Lines = K::Lines;
    held_to_the_table!();

    const HEADER: Listed = K::HEADER;

    const EMPTY_LINE_READS: bool = K::EMPTY_LINE_READS;

    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        let refused = |reason: &str| {
            SipParseError::new(format!("{} {reason}: {:?}", K::name(), raw.as_str()))
        };
        let (token, params) =
            read_token_params(raw, K::LEADING, K::PARAMS).map_err(|r| refused(&r))?;
        if let Some(missing) = K::REQUIRED.iter().find(|name| !params.has(name)) {
            return Err(refused(&format!("has no {missing} parameter")));
        }
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
    use crate::header::{AcceptRange, SubscriptionState};

    #[test]
    fn a_media_range_reads_its_token_and_parameters() {
        let range = AcceptRange::parse(&SipStr::owned("application/sdp;level=1")).unwrap();
        assert!(range.is("APPLICATION/SDP"));
        assert_eq!(range.param("level").and_then(ParamValue::as_str), Some("1"));
    }

    #[test]
    fn a_subscription_state_round_trips_its_parameters() {
        let raw = "terminated;reason=noresource;retry-after=30";
        let s = SubscriptionState::parse(&SipStr::owned(raw)).unwrap();
        assert_eq!(s.token(), "terminated");
        assert_eq!(s.to_wire(), raw);
    }

    /// RFC 4488 §4: a REFER that asks for no implicit subscription.
    #[test]
    fn a_refer_sub_reads_its_value() {
        let r = crate::header::ReferSub::parse(&SipStr::owned("false;x=1")).unwrap();
        assert!(r.is("false"));
        assert_eq!(
            crate::header::HeaderName::known("refer-sub").map(|n| n.to_string()).as_deref(),
            Some("Refer-Sub")
        );
    }

    /// RFC 3891: the replaced dialog is the leading Call-ID plus the two tags.
    #[test]
    fn a_replaces_reads_the_dialog_it_names() {
        let raw = "425928@bobster.example.org;to-tag=7743;from-tag=6472";
        let r = crate::header::Replaces::parse(&SipStr::owned(raw)).unwrap();
        assert_eq!(r.token(), "425928@bobster.example.org");
        assert_eq!(r.param("to-tag").and_then(ParamValue::as_str), Some("7743"));
        assert_eq!(r.param("from-tag").and_then(ParamValue::as_str), Some("6472"));
        assert_eq!(r.to_wire(), raw);
    }

    #[test]
    fn an_empty_value_is_rejected() {
        assert!(SubscriptionState::parse(&SipStr::owned("  ")).is_err());
    }
}
