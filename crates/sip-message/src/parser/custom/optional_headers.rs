//! The optional structured headers the parser reads eagerly and non-fatally.
//!
//! [`extract_optional_indexed`] parses every optional structured header the
//! dispatch pass located into an [`OptionalHeaders`] of `Result`s (ADR-0003: a
//! malformed optional header does not reject the message).

use super::header_index::HeaderIndex;
use super::structured_headers::top_level_comma_entries;
use crate::error::SipParseError;
use crate::header::{AddrForm, HeaderName, HeaderValue, NameAddr, RAck, ReferTo, UriRole};
use crate::sip_str::SipStr;
use crate::types::{OptionalHeaders, SipHeader};

// ---------------------------------------------------------------------------
// Optional header parsers (eager + non-fatal)
// ---------------------------------------------------------------------------

/// Parse a multi-value name-addr header (flattened across instances and
/// comma-separated entries), each entry by the strict name-addr reader, its
/// URI a [`UriRole::Identity`]. Any malformed entry → `Err`, and the header's
/// entries are not used (ADR-0053).
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
            let entry = v.reslice(entry);
            let addr = NameAddr::parse_as(&entry, AddrForm::NameAddrOrSpec, UriRole::Identity)
                .map_err(|e| SipParseError::new(format!("Strict {header_name}: {}", e.reason)))?;
            out.push(addr);
        }
    }
    Ok(out)
}

fn parse_rack_header(value: Option<&SipStr>) -> Result<Option<RAck>, SipParseError> {
    let Some(value) = value else {
        return Ok(None);
    };
    RAck::parse(value)
        .map(Some)
        .map_err(|e| SipParseError::new(format!("Malformed RAck: {}", e.reason)))
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
        rack: parse_rack_header(idx.rack.first),
        refer_to: parse_refer_to_header(idx.refer_to.first),
    }
}

/// Parse every optional structured header of a header list — the entry point
/// for callers that hold no [`HeaderIndex`].
pub fn extract_optional(headers: &[SipHeader]) -> OptionalHeaders {
    extract_optional_indexed(&HeaderIndex::of_lines(headers))
}
