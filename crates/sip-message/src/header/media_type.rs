//! [`MediaType`], the value of a Content-Type (RFC 3261 §20.15, §25.1):
//! `m-type SLASH m-subtype *( SEMI m-parameter )`, the two tokens read apart.

use crate::error::SipParseError;
use crate::sip_str::SipStr;

use super::cardinality::{held_to_the_table, lines};
use super::name::HeaderId;
use super::params::{read_params, ParamReading, ParamValue, Params};
use super::scan::{byte_at, clamped, skip_ws, sub, token_end};
use super::single_valued::Listed;
use super::value::HeaderValue;
use super::wire::Wire;

/// A media type: its type and subtype, and its parameters.
///
/// [`token`](Self::token) is the one form of the pair, `type/subtype` with the
/// SWS the grammar allows around the slash (`SLASH = SWS "/" SWS`) left out,
/// so two spellings of one type compare equal and render alike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaType {
    essence: SipStr,
    /// Where the slash stands in `essence`; its length when there is none.
    slash: usize,
    params: Params,
}

impl MediaType {
    /// The media type `essence` states, `type/subtype`, as this stack writes
    /// it: taken as given, never read. [`HeaderValue::parse`] is the reader.
    pub fn new(essence: impl Into<SipStr>) -> Self {
        let essence: SipStr = essence.into();
        let slash = essence.as_bytes().iter().position(|&b| b == b'/').unwrap_or(essence.len());
        Self { essence, slash, params: Params::new() }
    }

    /// `type/subtype`, the one form of the pair.
    pub fn token(&self) -> &str {
        self.essence.as_str()
    }

    /// The top-level type (`m-type`).
    pub fn type_name(&self) -> &str {
        clamped(self.essence.as_str(), 0, self.slash)
    }

    /// The subtype (`m-subtype`).
    pub fn subtype(&self) -> &str {
        clamped(self.essence.as_str(), self.slash + 1, self.essence.len())
    }

    /// Whether the pair is `essence`, case aside (§7.3.1).
    pub fn is(&self, essence: &str) -> bool {
        self.essence.eq_ignore_ascii_case(essence)
    }

    /// Whether the value names the SDP media type (RFC 4566 §8).
    pub fn is_sdp(&self) -> bool {
        self.is("application/sdp")
    }

    /// Whether the value names a `multipart/…` composite type (RFC 2046 §5.1).
    pub fn is_multipart(&self) -> bool {
        self.type_name().eq_ignore_ascii_case("multipart")
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

/// `m-type SLASH m-subtype` at the start of `value`: the pair in its one form
/// and where the parameters start.
fn read_essence(value: &SipStr) -> Result<(SipStr, usize, usize), String> {
    let bytes = value.as_bytes();
    let type_end = token_end(bytes, 0);
    let slash = skip_ws(bytes, type_end);
    let sub_at = skip_ws(bytes, slash + 1);
    let end = token_end(bytes, sub_at);
    if type_end == 0 || byte_at(bytes, slash) != Some(b'/') || end == sub_at {
        return Err("leading part is no type/subtype".to_string());
    }
    let essence = if slash == type_end && sub_at == slash + 1 {
        sub(value, 0, end)
    } else {
        let text = value.as_str();
        let (m_type, m_subtype) = (clamped(text, 0, type_end), clamped(text, sub_at, end));
        SipStr::owned(&format!("{m_type}/{m_subtype}"))
    };
    Ok((essence, type_end, end))
}

impl HeaderValue for MediaType {
    type Lines = lines::Single;
    held_to_the_table!();

    const HEADER: Listed = Listed::Known(HeaderId::ContentType);

    /// `m-parameter = m-attribute EQUAL m-value`: a parameter with no value
    /// does not read.
    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        let refused =
            |reason: &str| SipParseError::new(format!("Content-Type {reason}: {:?}", raw.as_str()));
        let value = raw.trimmed();
        let (essence, slash, params_at) = read_essence(&value).map_err(|r| refused(&r))?;
        let (params, _) = read_params(&value, params_at, ParamReading::Strict, None)
            .map_err(|e| refused(&e.reason))?;
        if let Some((name, _)) = params.iter().find(|(_, v)| v.is_flag()) {
            return Err(refused(&format!("parameter {:?} has no value", name.as_str())));
        }
        Ok(Self { essence, slash, params })
    }

    fn render(&self, out: &mut Wire) {
        out.str(self.essence.as_str());
        self.params.render(out);
    }
}

impl std::fmt::Display for MediaType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_wire())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<MediaType, SipParseError> {
        MediaType::parse(&SipStr::owned(raw))
    }

    #[test]
    fn a_media_type_classifies_by_its_tokens_not_by_value_prefix() {
        assert!(parse("application/sdp").unwrap().is_sdp());
        assert!(parse(" Application/SDP ; charset=utf-8").unwrap().is_sdp());
        assert!(!parse("application/sdp-x").unwrap().is_sdp(), "a longer token is another type");
        assert!(parse("multipart/mixed;boundary=b").unwrap().is_multipart());
        assert!(parse("Multipart/Related").unwrap().is_multipart());
        assert!(!parse("application/sdp").unwrap().is_multipart());
        assert!(!parse("multipartx/mixed").unwrap().is_multipart());
    }

    /// RFC 3261 §25.1: `SLASH = SWS "/" SWS`, so the spaces are no part of
    /// either token.
    #[test]
    fn the_space_around_the_slash_is_read_away() {
        for raw in ["application / sdp", "application/ sdp", "application\t/sdp"] {
            let media = parse(raw).unwrap();
            assert!(media.is_sdp(), "{raw}");
            assert_eq!(media.token(), "application/sdp");
            assert_eq!((media.type_name(), media.subtype()), ("application", "sdp"));
            assert_eq!(media.to_wire(), "application/sdp");
        }
    }

    #[test]
    fn a_media_type_reads_its_parameters() {
        let media = parse("multipart/mixed ; boundary=\"a b\";charset=x").unwrap();
        assert!(media.is("MULTIPART/MIXED"));
        assert_eq!(media.param("boundary").and_then(ParamValue::as_str), Some("a b"));
        assert_eq!(media.param("charset").and_then(ParamValue::as_str), Some("x"));
    }

    #[test]
    fn a_value_with_no_type_subtype_shape_does_not_read() {
        for raw in ["", "  ", "application", "application/", "/sdp", "a/b/c", "a b/c", "a/b c"] {
            assert!(parse(raw).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn a_parameter_with_no_value_does_not_read() {
        assert!(parse("multipart/mixed;boundary").is_err());
        assert!(parse("multipart/mixed;boundary=").is_err());
    }

    #[test]
    fn a_stated_media_type_names_its_halves() {
        let media = MediaType::new("application/sdp");
        assert_eq!((media.type_name(), media.subtype()), ("application", "sdp"));
        assert!(media.is_sdp());
        let odd = MediaType::new("nothing");
        assert_eq!((odd.type_name(), odd.subtype()), ("nothing", ""));
        assert!(!odd.is_multipart());
    }
}
