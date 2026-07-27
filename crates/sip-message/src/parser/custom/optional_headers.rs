//! Optional structured headers + strict re-parsers. Port of
//! `src/sip/parsers/custom/lazy-parsers.ts`.
//!
//! Two roles:
//!  1. [`extract_optional_indexed`] — eagerly + non-fatally parses every
//!     optional structured header the dispatch pass located into an
//!     [`OptionalHeaders`] of `Result`s (per docs/adr/0003: a malformed
//!     optional header does not reject the message).
//!  2. [`run_all_strict`] — the port of `runAllStrictLazyParsers`: re-validates
//!     Date/From/To/Contact grammar + every optional structured header and
//!     returns the first violation. Backs [`crate::types::SipMessage::validate_strict`].

use super::header_index::HeaderIndex;
use super::scanner::is_token_char;
use super::structured_headers::{
    find_uri_embedded_headers_start, parse_name_addr, top_level_comma_entries,
    validate_strict_sip_uri, ParsedNameAddr,
};
use crate::error::SipParseError;
use crate::header::{HeaderName, HeaderValue, NameAddr, RAck, ReferTo, Uri};
use crate::sip_str::SipStr;
use crate::types::{OptionalHeaders, SipHeader};

fn to_name_addr(p: ParsedNameAddr) -> NameAddr {
    NameAddr::from_parts(p.display_name, Uri::parse_or_opaque(&p.uri), p.params)
}

// ---------------------------------------------------------------------------
// Optional header parsers (eager + non-fatal)
// ---------------------------------------------------------------------------

/// Parse a multi-value name-addr header (flattened across instances and
/// comma-separated entries). Any malformed entry → `Err`.
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
            let parsed = parse_name_addr(&v.reslice(entry));
            if parsed.uri.is_empty() {
                return Err(SipParseError::new(format!("Malformed {header_name} entry: \"{entry}\"")));
            }
            if let Some(reason) = validate_strict_sip_uri(&parsed.uri) {
                return Err(SipParseError::new(format!(
                    "Strict {header_name} URI: {reason} (\"{}\")",
                    parsed.uri
                )));
            }
            out.push(to_name_addr(parsed));
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
    let parsed = ReferTo::parse(value)
        .map_err(|_| SipParseError::new(format!("Malformed Refer-To: \"{value}\"")))?;
    // Strict SIP-URI on the target URI head (without embedded headers).
    // `find_uri_embedded_headers_start` returns a byte index at the `?` (ASCII
    // -> char boundary), so the head is a plain borrow.
    let violation = {
        let text = parsed.uri().text();
        let uri_head = match find_uri_embedded_headers_start(&text) {
            None => &text[..],
            Some(q) => &text[..q],
        };
        validate_strict_sip_uri(uri_head)
            .map(|reason| format!("Strict Refer-To URI: {reason} (\"{uri_head}\")"))
    };
    match violation {
        Some(reason) => Err(SipParseError::new(reason)),
        None => Ok(Some(parsed)),
    }
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
// Strict re-parsers (Date / From / To / Contact)
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
    let dow: String = v[0..3].iter().collect();
    if !DOW.contains(&dow.as_str()) {
        return Err(SipParseError::new(format!("Malformed Date: bad day-of-week \"{dow}\"")));
    }
    if v[3] != ',' || v[4] != ' ' {
        return bad("missing \", \" after day-of-week");
    }
    if !v[5].is_ascii_digit() || !v[6].is_ascii_digit() || v[7] != ' ' {
        return bad("bad day-of-month");
    }
    let day: u32 = v[5..7].iter().collect::<String>().parse().unwrap_or(0);
    let mon: String = v[8..11].iter().collect();
    let mon_ok = MON.contains(&mon.as_str());
    if !mon_ok || v[11] != ' ' {
        return Err(SipParseError::new(format!("Malformed Date: bad month \"{mon}\"")));
    }
    for &c in &v[12..16] {
        if !c.is_ascii_digit() {
            return bad("bad year");
        }
    }
    if v[16] != ' ' {
        return bad("missing SP before time");
    }
    if !v[17].is_ascii_digit() || !v[18].is_ascii_digit() || v[19] != ':'
        || !v[20].is_ascii_digit() || !v[21].is_ascii_digit() || v[22] != ':'
        || !v[23].is_ascii_digit() || !v[24].is_ascii_digit() || v[25] != ' '
    {
        return bad("bad HH:MM:SS");
    }
    let hh: u32 = v[17..19].iter().collect::<String>().parse().unwrap_or(99);
    let mm: u32 = v[20..22].iter().collect::<String>().parse().unwrap_or(99);
    let ss: u32 = v[23..25].iter().collect::<String>().parse().unwrap_or(99);
    let tz: String = v[26..].iter().collect();
    if tz != "GMT" {
        return Err(SipParseError::new(format!("Malformed Date: expected \"GMT\", got \"{tz}\"")));
    }
    if !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 59 {
        return bad("out-of-range field");
    }
    Ok(())
}

fn parse_date_header_strict(values: &[&SipStr]) -> Result<(), SipParseError> {
    let Some(first) = values.first() else {
        return Ok(());
    };
    // sip-parser-style split at the day-of-week comma is rejoined with ", ".
    if values.len() == 1 {
        return parse_date_value_strict(first);
    }
    let joined = values.iter().map(|v| v.as_str()).collect::<Vec<_>>().join(", ");
    parse_date_value_strict(&joined)
}

fn is_token_char_c(c: char) -> bool {
    (c as u32) < 0x80 && is_token_char(c as u8)
}

/// Validate an UNQUOTED display name (tokens joined by LWS). `Bell, Alexander`
/// fails (`,` not a token char); `J Rosenberg` passes.
fn is_valid_unquoted_display_name(s: &str) -> bool {
    for c in s.chars() {
        if c == ' ' || c == '\t' {
            continue;
        }
        if !is_token_char_c(c) {
            return false;
        }
    }
    true
}

fn index_of(s: &[char], needle: char, from: usize) -> Option<usize> {
    (from..s.len()).find(|&j| s[j] == needle)
}

/// Validate the `<...>` envelope at `lt` (must be `<`): no LWS inside, must close.
fn validate_angle_section(s: &[char], lt: usize, header_name: &str) -> Result<(), SipParseError> {
    if s[lt] != '<' {
        return Err(SipParseError::new(format!("Strict {header_name}: expected \"<\"")));
    }
    let gt = match index_of(s, '>', lt + 1) {
        Some(g) => g,
        None => return Err(SipParseError::new(format!("Strict {header_name}: unterminated \"<...>\""))),
    };
    let first = s[lt + 1];
    if first == ' ' || first == '\t' {
        return Err(SipParseError::new(format!("Strict {header_name}: LWS inside \"<...>\" addr-spec")));
    }
    let last = s[gt - 1];
    if last == ' ' || last == '\t' {
        return Err(SipParseError::new(format!("Strict {header_name}: LWS inside \"<...>\" addr-spec")));
    }
    Ok(())
}

/// Re-scan a From/To/Contact value with stricter rules than `parse_name_addr`:
/// non-token unquoted display name (3.1.2.15), LWS inside `<...>` (3.1.2.14),
/// bare addr-spec carrying `?embedded` headers (3.1.2.13), missing scheme.
fn validate_name_addr_strict(value: &str, header_name: &str) -> Result<(), SipParseError> {
    let s: Vec<char> = value.chars().collect();
    let len = s.len();
    let mut i = 0;
    while i < len && (s[i] == ' ' || s[i] == '\t') {
        i += 1;
    }

    // Quoted display name: tolerate trailing bytes before `<` (wsinv), but the
    // `<...>` section (if present) must still be LWS-free.
    if i < len && s[i] == '"' {
        i += 1;
        let mut closed = false;
        while i < len {
            let c = s[i];
            if c == '\\' && i + 1 < len {
                i += 2;
                continue;
            }
            if c == '"' {
                i += 1;
                closed = true;
                break;
            }
            i += 1;
        }
        if !closed {
            return Err(SipParseError::new(format!("Strict {header_name}: unterminated quoted display name")));
        }
        return match index_of(&s, '<', i) {
            None => Ok(()),
            Some(lt) => validate_angle_section(&s, lt, header_name),
        };
    }

    // Unquoted display name before `<`.
    if let Some(lt) = index_of(&s, '<', i) {
        let dn: String = s[i..lt].iter().collect::<String>().trim_end().to_string();
        if !is_valid_unquoted_display_name(&dn) {
            return Err(SipParseError::new(format!(
                "Strict {header_name}: non-token char in unquoted display name \"{dn}\""
            )));
        }
        return validate_angle_section(&s, lt, header_name);
    }

    // Bare addr-spec. `?` in the URI head requires name-addr form (3.1.2.13).
    let semi_or_end = index_of(&s, ';', i).unwrap_or(len);
    for k in i..semi_or_end {
        if s[k] == '?' {
            return Err(SipParseError::new(format!(
                "Strict {header_name}: bare addr-spec with embedded \"?headers\"; name-addr \"<sip:...>\" form required"
            )));
        }
    }
    // An addr-spec MUST contain a scheme colon.
    let has_colon = (i..semi_or_end).any(|k| s[k] == ':');
    if !has_colon {
        let trimmed: String = s[i..].iter().collect::<String>().trim().to_string();
        if !trimmed.is_empty() {
            return Err(SipParseError::new(format!("Strict {header_name}: addr-spec required, got \"{trimmed}\"")));
        }
    }
    Ok(())
}

fn validate_contact_strict(values: &[&SipStr]) -> Result<(), SipParseError> {
    for v in values {
        for entry in top_level_comma_entries(v) {
            if entry.is_empty() {
                continue;
            }
            validate_name_addr_strict(entry, "Contact")?;
        }
    }
    Ok(())
}

/// Run every strict re-parser + optional-header parser; return the first
/// failure. Port of `runAllStrictLazyParsers`.
pub fn run_all_strict(headers: &[SipHeader]) -> Result<(), SipParseError> {
    let idx = HeaderIndex::build(headers);
    parse_date_header_strict(&idx.date)?;
    if let Some(from) = idx.from.first {
        validate_name_addr_strict(from, "From")?;
    }
    if let Some(to) = idx.to.first {
        validate_name_addr_strict(to, "To")?;
    }
    validate_contact_strict(&idx.contact)?;
    let opt = extract_optional_indexed(&idx);
    opt.p_asserted_identity?;
    opt.p_preferred_identity?;
    opt.diversion?;
    opt.history_info?;
    opt.remote_party_id?;
    opt.geolocation?;
    opt.geolocation_error?;
    opt.geolocation_routing?;
    opt.rack?;
    opt.refer_to?;
    Ok(())
}
