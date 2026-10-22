//! The RFC 6442 location headers: [`Geolocation`], [`GeolocationError`] and
//! [`GeolocationRouting`]. The parser reads none of them; a reader asks the
//! message for one through the typed read surface, and a malformed value is an
//! `Err` there only.

use crate::error::SipParseError;
use crate::sip_str::SipStr;

use super::name::HeaderName;
use super::name_addr::{AddrForm, NameAddr};
use super::params::{read_generic_params, Params};
use super::scan::{clamped, digits_end};
use super::uri::{Uri, UriRole};
use super::value::{Folding, HeaderValue};
use super::wire::Wire;

/// One location reference (RFC 6442 §4.1): `locationValue = LAQUOT
/// locationURI RAQUOT *(SEMI geoloc-param)`. A `cid:` URI names a body part
/// of the message; any other scheme names a location held elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Geolocation {
    uri: Uri,
    params: Params,
}

impl Geolocation {
    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    pub fn params(&self) -> &Params {
        &self.params
    }
}

impl HeaderValue for Geolocation {
    fn header_name() -> HeaderName {
        HeaderName::Geolocation
    }

    fn folding() -> Folding {
        Folding::Comma
    }

    /// The URI between angle brackets, read by the one URI reader, then the
    /// generic parameters: a display name or a bare URI is refused.
    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        let addr = NameAddr::parse_as(raw, AddrForm::NameAddrOnly, UriRole::Identity)
            .map_err(|e| SipParseError::new(format!("Strict Geolocation: {}", e.reason)))?;
        if addr.display().is_some() {
            return Err(SipParseError::new(format!(
                "Strict Geolocation: a locationValue carries no display name: {:?}",
                raw.as_str()
            )));
        }
        Ok(Self { uri: addr.uri().clone(), params: addr.params().clone() })
    }

    fn render(&self, out: &mut Wire) {
        out.byte(b'<');
        self.uri.render(out);
        out.byte(b'>');
        self.params.render(out);
    }
}

/// Why a location could not be used (RFC 6442 §4.3): `locationErrorValue =
/// location-error-code *(SEMI location-error-params)`, the code `1*3DIGIT`,
/// its text the `code` parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeolocationError {
    code: u16,
    params: Params,
}

impl GeolocationError {
    pub fn code(&self) -> u16 {
        self.code
    }

    /// The `code="…"` text, unquoted.
    pub fn text(&self) -> Option<&str> {
        self.params.value("code")
    }

    pub fn params(&self) -> &Params {
        &self.params
    }
}

impl HeaderValue for GeolocationError {
    fn header_name() -> HeaderName {
        HeaderName::GeolocationError
    }

    fn folding() -> Folding {
        Folding::Single
    }

    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        let value = raw.trimmed();
        let end = digits_end(value.as_bytes(), 0);
        let digits = clamped(value.as_str(), 0, end);
        let code = match digits.len() {
            1..=3 => digits.parse::<u16>().ok(),
            _ => None,
        }
        .ok_or_else(|| {
            SipParseError::new(format!(
                "Geolocation-Error code is not 1*3DIGIT: {:?}",
                value.as_str()
            ))
        })?;
        let params = read_generic_params(&value, end)
            .map_err(|e| SipParseError::new(format!("Geolocation-Error: {}", e.reason)))?;
        Ok(Self { code, params })
    }

    fn render(&self, out: &mut Wire) {
        out.num(u64::from(self.code));
        self.params.render(out);
    }
}

/// Whether the location may be used to route the request (RFC 6442 §4.2):
/// `"yes" / "no" / generic-value`. Only "yes" allows it; "no", and any value
/// this element does not understand, read as "no".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeolocationRouting(SipStr);

impl GeolocationRouting {
    /// Whether the value is "yes", case-insensitively.
    pub fn allowed(&self) -> bool {
        self.0.as_str().eq_ignore_ascii_case("yes")
    }
}

impl HeaderValue for GeolocationRouting {
    fn header_name() -> HeaderName {
        HeaderName::GeolocationRouting
    }

    fn folding() -> Folding {
        Folding::Single
    }

    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        let value = raw.trimmed();
        if value.is_empty() {
            return Err(SipParseError::new("Geolocation-Routing is empty"));
        }
        Ok(Self(value))
    }

    fn render(&self, out: &mut Wire) {
        out.str(self.0.as_str());
    }
}
