//! The optional structured headers the parser reads eagerly and non-fatally.
//!
//! [`extract_optional_indexed`] parses every optional structured header the
//! dispatch pass located into an [`OptionalHeaders`] of `Result`s (ADR-0003: a
//! malformed optional header does not reject the message).

use super::header_index::HeaderIndex;
use super::structured_headers::top_level_comma_entries;
use crate::error::SipParseError;
use crate::header::{HeaderName, HeaderValue, NameAddr, RAck, ReferTo};
use crate::sip_str::SipStr;
use crate::types::{OptionalHeaders, SipHeader};

// ---------------------------------------------------------------------------
// Optional header parsers (eager + non-fatal)
// ---------------------------------------------------------------------------

/// Parse a multi-value name-addr header (flattened across instances and
/// comma-separated entries), each entry by the strict name-addr reader. Any
/// malformed entry → `Err`.
fn parse_name_addr_list(
    values: &[&SipStr],
    header_name: HeaderName,
) -> Result<Vec<NameAddr>, SipParseError> {
    let mut out = Vec::new();
    for v in values {
        for entry in top_level_comma_entries(v.as_str()) {
            if entry.is_empty() {
                continue;
            }
            let addr = NameAddr::parse(&v.reslice(entry))
                .map_err(|e| SipParseError::new(format!("Strict {header_name}: {}", e.reason)))?;
            out.push(addr);
        }
    }
    Ok(out)
}

/// `Geolocation-Routing` (RFC 6442 §4.2): token "yes"/"no". Absent → `Ok(None)`.
fn parse_geolocation_routing(value: Option<&SipStr>) -> Result<Option<bool>, SipParseError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let v = value.trim().to_lowercase();
    match v.as_str() {
        "yes" => Ok(Some(true)),
        "no" => Ok(Some(false)),
        _ => Err(SipParseError::new(format!("Invalid Geolocation-Routing value: \"{v}\""))),
    }
}

fn parse_rack_header(value: Option<&SipStr>) -> Result<Option<RAck>, SipParseError> {
    let Some(value) = value else {
        return Ok(None);
    };
    RAck::parse(value)
        .map(Some)
        .map_err(|_| SipParseError::new(format!("Malformed RAck: \"{value}\"")))
}

fn parse_refer_to_header(value: Option<&SipStr>) -> Result<Option<ReferTo>, SipParseError> {
    let Some(value) = value else {
        return Ok(None);
    };
    ReferTo::parse(value)
        .map(Some)
        .map_err(|e| SipParseError::new(format!("Malformed Refer-To: {}", e.reason)))
}

/// Parse every optional structured header eagerly + non-fatally, from the
/// values the one dispatch pass already located.
pub fn extract_optional_indexed(idx: &HeaderIndex) -> OptionalHeaders {
    OptionalHeaders {
        p_asserted_identity: parse_name_addr_list(
            &idx.p_asserted_identity,
            HeaderName::PAssertedIdentity,
        ),
        p_preferred_identity: parse_name_addr_list(
            &idx.p_preferred_identity,
            HeaderName::PPreferredIdentity,
        ),
        diversion: parse_name_addr_list(&idx.diversion, HeaderName::Diversion),
        history_info: parse_name_addr_list(&idx.history_info, HeaderName::HistoryInfo),
        remote_party_id: parse_name_addr_list(&idx.remote_party_id, HeaderName::RemotePartyId),
        geolocation: parse_name_addr_list(&idx.geolocation, HeaderName::Geolocation),
        geolocation_error: parse_name_addr_list(
            &idx.geolocation_error,
            HeaderName::GeolocationError,
        ),
        geolocation_routing: parse_geolocation_routing(idx.geolocation_routing.first),
        rack: parse_rack_header(idx.rack.first),
        refer_to: parse_refer_to_header(idx.refer_to.first),
    }
}

/// Parse every optional structured header of a header list — the entry point
/// for callers that hold no [`HeaderIndex`].
pub fn extract_optional(headers: &[SipHeader]) -> OptionalHeaders {
    extract_optional_indexed(&HeaderIndex::build(headers))
}
