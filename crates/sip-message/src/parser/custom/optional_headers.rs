//! The optional structured headers the parser reads eagerly and non-fatally,
//! and the Date gate.
//!
//! [`extract_optional_indexed`] parses every optional structured header the
//! dispatch pass located into an [`OptionalHeaders`] of `Result`s (ADR-0003: a
//! malformed optional header does not reject the message). The Date gate
//! holds a Date on wire bytes to its RFC 3261 §20.17 form.

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

// ---------------------------------------------------------------------------
// The Date gate
// ---------------------------------------------------------------------------

const DOW: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const MON: [&str; 12] =
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// RFC 3261 §20.17 strict Date: `Day, DD Mon YYYY HH:MM:SS GMT`. GMT literal
/// is mandatory; any other zone is a syntax violation.
fn parse_date_value_strict(value: &str) -> Result<(), SipParseError> {
    let v: Vec<char> = value.trim().chars().collect();
    let bad = |msg: &str| Err(SipParseError::new(format!("Malformed Date: {msg}")));
    if v.len() < 29 {
        let s: String = v.iter().collect();
        return Err(SipParseError::new(format!("Malformed Date: too short \"{s}\"")));
    }
    let at = |i: usize| v.get(i).copied();
    let digit = |i: usize| at(i).is_some_and(|c| c.is_ascii_digit());
    let text = |from: usize, to: usize| -> String { v.iter().take(to).skip(from).collect() };
    let dow = text(0, 3);
    if !DOW.contains(&dow.as_str()) {
        return Err(SipParseError::new(format!("Malformed Date: bad day-of-week \"{dow}\"")));
    }
    if at(3) != Some(',') || at(4) != Some(' ') {
        return bad("missing \", \" after day-of-week");
    }
    if !digit(5) || !digit(6) || at(7) != Some(' ') {
        return bad("bad day-of-month");
    }
    let day: u32 = text(5, 7).parse().unwrap_or(0);
    let mon = text(8, 11);
    let mon_ok = MON.contains(&mon.as_str());
    if !mon_ok || at(11) != Some(' ') {
        return Err(SipParseError::new(format!("Malformed Date: bad month \"{mon}\"")));
    }
    if !(12..16).all(digit) {
        return bad("bad year");
    }
    if at(16) != Some(' ') {
        return bad("missing SP before time");
    }
    if !digit(17)
        || !digit(18)
        || at(19) != Some(':')
        || !digit(20)
        || !digit(21)
        || at(22) != Some(':')
        || !digit(23)
        || !digit(24)
        || at(25) != Some(' ')
    {
        return bad("bad HH:MM:SS");
    }
    let hh: u32 = text(17, 19).parse().unwrap_or(99);
    let mm: u32 = text(20, 22).parse().unwrap_or(99);
    let ss: u32 = text(23, 25).parse().unwrap_or(99);
    let tz: String = v.iter().skip(26).collect();
    if tz != "GMT" {
        return Err(SipParseError::new(format!("Malformed Date: expected \"GMT\", got \"{tz}\"")));
    }
    if !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 59 {
        return bad("out-of-range field");
    }
    Ok(())
}

/// Why the Date the message carries is not of the RFC 3261 §20.17 form, if
/// it is not. Several Date lines are read as one, joined with `, `.
pub(crate) fn date_refusal(values: &[&SipStr]) -> Result<(), SipParseError> {
    let Some(first) = values.first() else {
        return Ok(());
    };
    if values.len() == 1 {
        return parse_date_value_strict(first);
    }
    let joined = values.iter().map(|v| v.as_str()).collect::<Vec<_>>().join(", ");
    parse_date_value_strict(&joined)
}
