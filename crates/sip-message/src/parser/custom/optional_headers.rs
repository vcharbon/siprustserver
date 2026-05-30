//! Optional structured headers + strict re-parsers. Port of
//! `src/sip/parsers/custom/lazy-parsers.ts`.
//!
//! Two roles:
//!  1. [`extract_optional`] — eagerly + non-fatally parses every optional
//!     structured header into an [`OptionalHeaders`] of `Result`s (per
//!     docs/adr/0003: a malformed optional header does not reject the message).
//!  2. [`run_all_strict`] — the port of `runAllStrictLazyParsers`: re-validates
//!     Date/From/To/Contact grammar + every optional structured header and
//!     returns the first violation. Backs [`crate::types::SipMessage::validate_strict`].

use super::scanner::is_token_char;
use super::structured_headers::{
    find_uri_embedded_headers_start, parse_name_addr, parse_rack, parse_refer_to,
    split_top_level_commas, validate_strict_sip_uri, ParsedNameAddr, ParsedReferTo,
};
use crate::error::SipParseError;
use crate::types::{NameAddr, OptionalHeaders, Rack, ReferTo, Replaces, SipHeader, Uri};

fn get_header_values<'a>(headers: &'a [SipHeader], name: &str) -> Vec<&'a str> {
    let lower = name.to_lowercase();
    headers
        .iter()
        .filter(|h| h.name.to_lowercase() == lower)
        .map(|h| h.value.as_str())
        .collect()
}

fn to_name_addr(p: ParsedNameAddr) -> NameAddr {
    NameAddr { display_name: p.display_name, uri: p.uri, tag: p.tag, params: p.params }
}

// ---------------------------------------------------------------------------
// Optional header parsers (eager + non-fatal)
// ---------------------------------------------------------------------------

/// Parse a multi-value name-addr header (flattened across instances and
/// comma-separated entries). Any malformed entry → `Err`.
fn parse_name_addr_list(headers: &[SipHeader], header_name: &str) -> Result<Vec<NameAddr>, SipParseError> {
    let values = get_header_values(headers, header_name);
    if values.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for v in values {
        for entry in split_top_level_commas(v) {
            if entry.is_empty() {
                continue;
            }
            let parsed = parse_name_addr(&entry);
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
fn parse_geolocation_routing(headers: &[SipHeader]) -> Result<Option<bool>, SipParseError> {
    let values = get_header_values(headers, "Geolocation-Routing");
    if values.is_empty() {
        return Ok(None);
    }
    let v = values[0].trim().to_lowercase();
    match v.as_str() {
        "yes" => Ok(Some(true)),
        "no" => Ok(Some(false)),
        _ => Err(SipParseError::new(format!("Invalid Geolocation-Routing value: \"{v}\""))),
    }
}

fn parse_rack_header(headers: &[SipHeader]) -> Result<Option<Rack>, SipParseError> {
    let values = get_header_values(headers, "RAck");
    if values.is_empty() {
        return Ok(None);
    }
    match parse_rack(values[0]) {
        Some(r) => Ok(Some(Rack { rseq: r.rseq, seq: r.seq, method: r.method })),
        None => Err(SipParseError::new(format!("Malformed RAck: \"{}\"", values[0]))),
    }
}

fn to_refer_to(rt: ParsedReferTo) -> ReferTo {
    ReferTo {
        display_name: rt.display_name,
        uri: rt.uri,
        parsed_uri: rt.parsed_uri.map(|u| Uri {
            scheme: u.scheme,
            user: u.user,
            host: u.host,
            port: u.port,
            params: u.params,
        }),
        params: rt.params,
        embedded_headers: rt.embedded_headers,
        replaces: rt.replaces.map(|r| Replaces {
            call_id: r.call_id,
            to_tag: r.to_tag,
            from_tag: r.from_tag,
            early_only: r.early_only,
        }),
    }
}

fn parse_refer_to_header(headers: &[SipHeader]) -> Result<Option<ReferTo>, SipParseError> {
    let values = get_header_values(headers, "Refer-To");
    if values.is_empty() {
        return Ok(None);
    }
    let parsed = match parse_refer_to(values[0]) {
        Some(rt) => rt,
        None => return Err(SipParseError::new(format!("Malformed Refer-To: \"{}\"", values[0]))),
    };
    // Strict SIP-URI on the target URI head (without embedded headers).
    let uri_chars: Vec<char> = parsed.uri.chars().collect();
    let uri_head: String = match find_uri_embedded_headers_start(&parsed.uri) {
        None => parsed.uri.clone(),
        Some(q) => uri_chars[..q].iter().collect(),
    };
    if let Some(reason) = validate_strict_sip_uri(&uri_head) {
        return Err(SipParseError::new(format!("Strict Refer-To URI: {reason} (\"{uri_head}\")")));
    }
    Ok(Some(to_refer_to(parsed)))
}

/// Parse every optional structured header eagerly + non-fatally.
pub fn extract_optional(headers: &[SipHeader]) -> OptionalHeaders {
    OptionalHeaders {
        p_asserted_identity: parse_name_addr_list(headers, "P-Asserted-Identity"),
        p_preferred_identity: parse_name_addr_list(headers, "P-Preferred-Identity"),
        diversion: parse_name_addr_list(headers, "Diversion"),
        history_info: parse_name_addr_list(headers, "History-Info"),
        remote_party_id: parse_name_addr_list(headers, "Remote-Party-ID"),
        geolocation: parse_name_addr_list(headers, "Geolocation"),
        geolocation_error: parse_name_addr_list(headers, "Geolocation-Error"),
        geolocation_routing: parse_geolocation_routing(headers),
        rack: parse_rack_header(headers),
        refer_to: parse_refer_to_header(headers),
    }
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
    if day < 1 || day > 31 || hh > 23 || mm > 59 || ss > 59 {
        return bad("out-of-range field");
    }
    Ok(())
}

fn parse_date_header_strict(headers: &[SipHeader]) -> Result<(), SipParseError> {
    let values = get_header_values(headers, "Date");
    if values.is_empty() {
        return Ok(());
    }
    // sip-parser-style split at the day-of-week comma is rejoined with ", ".
    let joined = if values.len() == 1 { values[0].to_string() } else { values.join(", ") };
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

fn validate_from_strict(headers: &[SipHeader]) -> Result<(), SipParseError> {
    match get_header_values(headers, "From").first() {
        None => Ok(()),
        Some(v) => validate_name_addr_strict(v, "From"),
    }
}

fn validate_to_strict(headers: &[SipHeader]) -> Result<(), SipParseError> {
    match get_header_values(headers, "To").first() {
        None => Ok(()),
        Some(v) => validate_name_addr_strict(v, "To"),
    }
}

fn validate_contact_strict(headers: &[SipHeader]) -> Result<(), SipParseError> {
    for v in get_header_values(headers, "Contact") {
        for entry in split_top_level_commas(v) {
            if entry.is_empty() {
                continue;
            }
            validate_name_addr_strict(&entry, "Contact")?;
        }
    }
    Ok(())
}

/// Run every strict re-parser + optional-header parser; return the first
/// failure. Port of `runAllStrictLazyParsers`.
pub fn run_all_strict(headers: &[SipHeader]) -> Result<(), SipParseError> {
    parse_date_header_strict(headers)?;
    validate_from_strict(headers)?;
    validate_to_strict(headers)?;
    validate_contact_strict(headers)?;
    let opt = extract_optional(headers);
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
