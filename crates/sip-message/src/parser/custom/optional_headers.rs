//! The optional structured headers the parser reads eagerly and non-fatally.
//!
//! [`extract_optional_indexed`] parses every optional structured header the
//! dispatch pass located into an [`OptionalHeaders`] of `Result`s (ADR-0003: a
//! malformed optional header does not reject the message).

use super::header_index::HeaderIndex;
use crate::error::SipParseError;
use crate::header::kind::{self, NameAddrKind};
use crate::header::{HeaderValue, NameAddr, NameAddrHeader, RAck, ReferTo};
use crate::sip_str::SipStr;
use crate::types::{OptionalHeaders, SipHeader};

// ---------------------------------------------------------------------------
// Optional header parsers (eager + non-fatal)
// ---------------------------------------------------------------------------

/// Read a multi-value name-addr header (flattened across instances and
/// comma-separated entries) by its own typed reader, so this reading and
/// `list::<K>()` agree. Any malformed entry, an empty one included, → `Err`,
/// and the header's entries are not used (ADR-0053). The list holds no more
/// room than its entries.
fn parse_name_addr_list<K: NameAddrKind>(
    values: &[&SipStr],
) -> Result<Vec<NameAddr>, SipParseError> {
    let mut read: Vec<NameAddrHeader<K>> = Vec::with_capacity(values.len());
    for v in values {
        NameAddrHeader::<K>::parse_line_into(v, &mut read)
            .map_err(|e| SipParseError::new(format!("Strict {}: {}", K::name(), e.reason)))?;
    }
    Ok(read.into_iter().map(NameAddrHeader::into_addr).collect())
}

fn parse_rack_header(value: Option<&SipStr>) -> Result<Option<RAck>, SipParseError> {
    let Some(value) = value else {
        return Ok(None);
    };
    RAck::parse(value)
        .map(Some)
        .map_err(|e| SipParseError::new(format!("Malformed RAck: {}", e.reason)))
}

fn parse_refer_to_header(value: Option<&SipStr>) -> Result<Option<Box<ReferTo>>, SipParseError> {
    let Some(value) = value else {
        return Ok(None);
    };
    ReferTo::parse(value)
        .map(|refer_to| Some(Box::new(refer_to)))
        .map_err(|e| SipParseError::new(format!("Malformed Refer-To: {}", e.reason)))
}

/// Parse every optional structured header eagerly + non-fatally, from the
/// values the one dispatch pass already located.
pub fn extract_optional_indexed(idx: &HeaderIndex) -> OptionalHeaders {
    OptionalHeaders {
        p_asserted_identity: parse_name_addr_list::<kind::PAssertedIdentity>(
            &idx.p_asserted_identity,
        ),
        p_preferred_identity: parse_name_addr_list::<kind::PPreferredIdentity>(
            &idx.p_preferred_identity,
        ),
        diversion: parse_name_addr_list::<kind::Diversion>(&idx.diversion),
        history_info: parse_name_addr_list::<kind::HistoryInfo>(&idx.history_info),
        remote_party_id: parse_name_addr_list::<kind::RemotePartyId>(&idx.remote_party_id),
        rack: parse_rack_header(idx.rack.value),
        refer_to: parse_refer_to_header(idx.refer_to.value),
    }
}

/// Parse every optional structured header of a header list — the entry point
/// for callers that hold no [`HeaderIndex`].
pub fn extract_optional(headers: &[SipHeader]) -> OptionalHeaders {
    extract_optional_indexed(&HeaderIndex::of_lines(headers))
}
