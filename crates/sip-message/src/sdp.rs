//! Minimal SDP helpers for the REFER / blind-transfer and fake-PRACK flows.
//! Port of `src/sip/SdpUtils.ts` + `src/sip/SdpAnswerFromOffer.ts`.
//!
//! Deliberately string-based — no external SDP library, not a full parser:
//! only the fields the B2BUA needs. Two tolerance levels coexist:
//!   - [`extract_codec_profile`] / [`build_answer_from_offer`] degrade
//!     gracefully (the core transfer path prefers this mid-call), while
//!   - [`validate_sdp_body`] enforces the RFC 4566 §5 minimum grammar for
//!     callers that must refuse a malformed body.

use std::collections::BTreeMap;

const CRLF: &str = "\r\n";

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Split into non-empty lines, tolerating CRLF or LF endings — mirrors the TS
/// `split(/\r\n|\n/).filter(l => l.length > 0)`.
pub(crate) fn split_lines(text: &str) -> Vec<&str> {
    text.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .filter(|l| !l.is_empty())
        .collect()
}

/// JS `Number.parseInt(s, 10)` semantics: optional sign, leading digits, stop
/// at the first non-digit; `None` when no digits are present (the NaN case).
fn parse_int_js(s: &str) -> Option<i64> {
    let s = s.trim_start();
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut neg = false;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        neg = bytes[i] == b'-';
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    let n: i64 = s[start..i].parse().ok()?;
    Some(if neg { -n } else { n })
}

/// Split a value on runs of ASCII whitespace, dropping empties — the port of
/// `trim().split(/\s+/).filter(t => t.length > 0)`.
fn ws_tokens(s: &str) -> Vec<&str> {
    s.split_whitespace().collect()
}

// ===========================================================================
// Strict SDP body validation — RFC 4566 §5 minimum grammar
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdpValidationError {
    pub reason: String,
}

impl SdpValidationError {
    fn new(reason: impl Into<String>) -> Self {
        Self { reason: reason.into() }
    }
}

/// Validate the minimum RFC 4566 §5 grammar of an SDP body. Returns `Ok(())`
/// or the first violation found. Empty body fails (`v=` absent). Content-Type
/// is the caller's responsibility — invoke only for `application/sdp`.
pub fn validate_sdp_body(body: &[u8]) -> Result<(), SdpValidationError> {
    let text = String::from_utf8_lossy(body);
    let lines = split_lines(&text);

    if lines.is_empty() {
        return Err(SdpValidationError::new("empty SDP body"));
    }

    for (prefix, reason) in [
        ("v=", "missing v= line"),
        ("o=", "missing o= line"),
        ("s=", "missing s= line"),
        ("t=", "missing t= line"),
    ] {
        if !lines.iter().any(|l| l.starts_with(prefix)) {
            return Err(SdpValidationError::new(reason));
        }
    }

    // v=0 — RFC 4566 §5.1 fixes the version.
    let v_line = lines.iter().find(|l| l.starts_with("v=")).unwrap();
    if *v_line != "v=0" {
        return Err(SdpValidationError::new(format!(
            "non-zero protocol-version: \"{v_line}\""
        )));
    }

    // o= must have exactly six SP-tokens.
    let o_line = lines.iter().find(|l| l.starts_with("o=")).unwrap();
    let o_fields = ws_tokens(&o_line[2..]);
    if o_fields.len() != 6 {
        return Err(SdpValidationError::new(format!(
            "o= line has {} tokens (want 6): \"{o_line}\"",
            o_fields.len()
        )));
    }

    // Every m= line has ≥ 4 tokens (<media> <port> <proto> <fmt>+).
    for line in &lines {
        if !line.starts_with("m=") {
            continue;
        }
        let tokens = ws_tokens(&line[2..]);
        if tokens.len() < 4 {
            return Err(SdpValidationError::new(format!(
                "m= line has {} tokens (want ≥ 4): \"{line}\"",
                tokens.len()
            )));
        }
    }

    Ok(())
}

/// Validate a body against the RFC 3264 offer/answer MODEL on top of the
/// RFC 4566 §5 grammar [`validate_sdp_body`] enforces. Returns `Ok(())`, or the
/// FIRST concrete failure — the layers, in the order they are asked:
///
///   - exactly one session description (one `v=` line), and it is `v=0`;
///   - the §5 grammar itself (`o=`/`s=`/`t=` presence, six-token `o=`, m-line
///     arity);
///   - `o=` sess-id and sess-version are non-negative integers, read as opaque
///     digit strings — RFC 4566 §5.2 bounds neither below 64 bits, and the
///     recommended NTP timestamp is exactly that wide;
///   - every `m=` block has a `c=` — its own or the session's — and a
///     non-negative-integer port;
///   - every `a=ptime:N` states `N > 0`.
///
/// An EMPTY body passes: it carries no description to reject. Content-Type is
/// the caller's responsibility — invoke only for `application/sdp`.
pub fn validate_offer_answer_body(body: &[u8]) -> Result<(), SdpValidationError> {
    if body.is_empty() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(body).into_owned();
    let lines = split_lines(&text);

    // Exactly one session description (one `v=` line), and it must be `v=0`.
    let v_lines: Vec<&&str> = lines.iter().filter(|l| l.starts_with("v=")).collect();
    if v_lines.is_empty() {
        return Err(SdpValidationError::new("missing v= line"));
    }
    if v_lines.len() > 1 {
        return Err(SdpValidationError::new(format!(
            "{} session descriptions (v= lines) — exactly one required",
            v_lines.len()
        )));
    }
    if *v_lines[0] != "v=0" {
        return Err(SdpValidationError::new(format!(
            "unexpected v= value '{}' (expected v=0)",
            v_lines[0]
        )));
    }

    validate_sdp_body(body)?;

    // o= sess-id / sess-version bounds. The six-token o= line is there:
    // `validate_sdp_body` has just confirmed it.
    let o_line = lines.iter().find(|l| l.starts_with("o=")).unwrap();
    let o_tokens = ws_tokens(&o_line[2..]);
    let fields = [("sess-id", o_tokens[1]), ("sess-version", o_tokens[2])];
    for (label, token) in fields {
        if token.is_empty() || !token.bytes().all(|b| b.is_ascii_digit()) {
            return Err(SdpValidationError::new(format!(
                "o= {label} '{token}' is not a non-negative integer"
            )));
        }
    }

    // Walk m= blocks: a session-level c= (before the first m=) satisfies the
    // c=-presence requirement for every block; otherwise each block needs its
    // own c= before the next m= boundary. Each m= line needs an integer port.
    let mut session_level_c = false;
    let mut in_media = false;
    let mut current_media_has_c = false;
    let mut current_media_name = String::new();
    let missing_c = |name: &str| {
        SdpValidationError::new(format!("m={name} block has no c= line and no session-level c="))
    };
    for line in &lines {
        if let Some(m_val) = line.strip_prefix("m=") {
            if in_media && !current_media_has_c && !session_level_c {
                return Err(missing_c(&current_media_name));
            }
            let m_tokens = ws_tokens(m_val);
            if m_tokens.len() < 3 {
                return Err(SdpValidationError::new(format!(
                    "m= line '{line}' has fewer than 3 tokens (expected: media port proto fmt...)"
                )));
            }
            let port_tok = m_tokens[1];
            if port_tok.is_empty() || !port_tok.bytes().all(|b| b.is_ascii_digit()) {
                return Err(SdpValidationError::new(format!(
                    "m= line port '{port_tok}' is not a non-negative integer"
                )));
            }
            in_media = true;
            current_media_has_c = false;
            current_media_name = m_tokens[0].to_string();
            continue;
        }
        if line.starts_with("c=") {
            if in_media {
                current_media_has_c = true;
            } else {
                session_level_c = true;
            }
        }
        if let Some(raw) = line.strip_prefix("a=ptime:") {
            // Digit string with a non-zero digit — positive without an integer
            // width the value would have to fit.
            let raw = raw.trim();
            let positive_integer =
                !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) && raw.bytes().any(|b| b != b'0');
            if !positive_integer {
                return Err(SdpValidationError::new(format!("a=ptime:{raw} is not > 0")));
            }
        }
    }
    if in_media && !current_media_has_c && !session_level_c {
        return Err(missing_c(&current_media_name));
    }
    Ok(())
}

/// True iff a `c=` line VALUE (no leading `c=`) names the unspecified address —
/// `IN IP4 0.0.0.0` or `IN IP6 ::`, the legacy hold idiom of RFC 3264 §8.4. A
/// trailing `/ttl` or `/count` on the address still names it.
pub fn c_line_is_unspecified(c_value: &str) -> bool {
    let toks = ws_tokens(c_value);
    if toks.len() < 3 || !toks[0].eq_ignore_ascii_case("IN") {
        return false;
    }
    let addr = toks[2].split('/').next().unwrap_or_default();
    (toks[1].eq_ignore_ascii_case("IP4") && addr == "0.0.0.0")
        || (toks[1].eq_ignore_ascii_case("IP6") && ip6_is_unspecified(addr))
}

/// True iff `addr` spells the IPv6 unspecified address. RFC 4291 §2.2 compresses
/// any run of zero groups to `::`, so `::` and `0:0:0:0:0:0:0:0` are one address:
/// every group reads zero and nothing else is written.
fn ip6_is_unspecified(addr: &str) -> bool {
    addr.contains(':') && addr.bytes().all(|b| b == b':' || b == b'0')
}

// ===========================================================================
// Codec profile extraction + held SDP construction (SdpUtils.ts)
// ===========================================================================

/// Codec profile extracted from the first audio m-line of an SDP body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecProfile {
    /// Media type from the m-line (e.g. "audio").
    pub media: String,
    /// Payload types in m-line order.
    pub payload_types: Vec<i64>,
    /// rtpmap attribute lines for the payload types, original order.
    pub rtpmaps: Vec<String>,
    /// fmtp attribute lines for the payload types, original order.
    pub fmtp: Vec<String>,
    /// ptime attribute line, if present.
    pub ptime: Option<String>,
    /// maxptime attribute line, if present.
    pub maxptime: Option<String>,
}

/// Extract the codec profile from the first audio m-section. Returns `None`
/// when the body has no parsable audio m-line.
pub fn extract_codec_profile(body: &[u8]) -> Option<CodecProfile> {
    let text = String::from_utf8_lossy(body);
    let lines = split_lines(&text);

    let mut m_line_idx: Option<usize> = None;
    let mut media = String::new();
    let mut payload_types: Vec<i64> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        if !line.starts_with("m=") {
            continue;
        }
        let parts = ws_tokens(&line[2..]);
        if parts.len() < 4 {
            continue;
        }
        if parts[0] != "audio" {
            continue;
        }
        let pts: Vec<i64> = parts[3..].iter().filter_map(|f| parse_int_js(f)).collect();
        if pts.is_empty() {
            continue;
        }
        media = parts[0].to_string();
        payload_types = pts;
        m_line_idx = Some(i);
        break;
    }

    let start = m_line_idx?;

    let allowed: std::collections::BTreeSet<i64> = payload_types.iter().copied().collect();
    let mut rtpmaps: Vec<String> = Vec::new();
    let mut fmtp: Vec<String> = Vec::new();
    let mut ptime: Option<String> = None;
    let mut maxptime: Option<String> = None;

    for line in &lines[start + 1..] {
        if line.starts_with("m=") {
            break;
        }
        if !line.starts_with("a=") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            let rest = rest.trim();
            if let Some(space) = rest.find(' ') {
                if let Some(pt) = parse_int_js(&rest[..space]) {
                    if allowed.contains(&pt) {
                        rtpmaps.push((*line).to_string());
                    }
                }
            }
        } else if let Some(rest) = line.strip_prefix("a=fmtp:") {
            let rest = rest.trim();
            if let Some(space) = rest.find(' ') {
                if let Some(pt) = parse_int_js(&rest[..space]) {
                    if allowed.contains(&pt) {
                        fmtp.push((*line).to_string());
                    }
                }
            }
        } else if line.starts_with("a=ptime:") {
            if ptime.is_none() {
                ptime = Some((*line).to_string());
            }
        } else if line.starts_with("a=maxptime:") && maxptime.is_none() {
            maxptime = Some((*line).to_string());
        }
    }

    Some(CodecProfile { media, payload_types, rtpmaps, fmtp, ptime, maxptime })
}

/// Options for [`build_held_sdp_from_profile`].
pub struct BuildHeldSdpOptions {
    /// B2BUA's local SDP-origin address (`0.0.0.0`/`::` → `127.0.0.1`).
    pub local_ip: String,
    /// Wall-clock millis used to derive the `o=` sess-id / sess-version.
    pub now_ms: i64,
}

/// Build a synthetic held SDP offer carrying `profile`'s codec list with the
/// m-line port set to 0 and `a=inactive` (RFC 3264 §5.1).
pub fn build_held_sdp_from_profile(profile: &CodecProfile, options: &BuildHeldSdpOptions) -> Vec<u8> {
    let origin_ip = sdp_origin_address(&options.local_ip);
    let sess_id = sdp_session_id(options.now_ms);
    let pts = profile
        .payload_types
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let mut lines: Vec<String> = vec![
        "v=0".to_string(),
        format!("o=b2bua {sess_id} {sess_id} IN IP4 {origin_ip}"),
        "s=-".to_string(),
        format!("c=IN IP4 {origin_ip}"),
        "t=0 0".to_string(),
        format!("m={} 0 RTP/AVP {}", profile.media, pts),
    ];
    lines.extend(profile.rtpmaps.iter().cloned());
    lines.extend(profile.fmtp.iter().cloned());
    if let Some(p) = &profile.ptime {
        lines.push(p.clone());
    }
    if let Some(m) = &profile.maxptime {
        lines.push(m.clone());
    }
    lines.push("a=inactive".to_string());
    let mut s = lines.join(CRLF);
    s.push_str(CRLF);
    s.into_bytes()
}

// ===========================================================================
// SDP answer construction (SdpAnswerFromOffer.ts)
// ===========================================================================

/// RFC 3551 static payload types recognised when an m-line has no rtpmap.
fn static_pt(pt: i64) -> Option<&'static str> {
    Some(match pt {
        0 => "PCMU/8000",
        3 => "GSM/8000",
        4 => "G723/8000",
        5 => "DVI4/8000",
        6 => "DVI4/16000",
        7 => "LPC/8000",
        8 => "PCMA/8000",
        9 => "G722/8000",
        13 => "CN/8000",
        15 => "G728/8000",
        18 => "G729/8000",
        _ => return None,
    })
}

struct MediaSection {
    media: String,
    port: i64,
    proto: String,
    payload_types: Vec<i64>,
    connection: Option<String>,
    rtpmaps: BTreeMap<i64, String>,
    fmtps: BTreeMap<i64, String>,
    direction: Option<String>,
}

struct ParsedSdp {
    session_connection: Option<String>,
    media_sections: Vec<MediaSection>,
}

/// The outcome of [`build_answer_from_offer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SdpBuildResult {
    Ok(Vec<u8>),
    NoCommonCodec { m_line_index: usize },
    NoAliceSdp,
}

/// Options for [`build_answer_from_offer`].
pub struct BuildAnswerOptions {
    pub local_ip: String,
    pub now_ms: i64,
}

fn parse_sdp(text: &str) -> ParsedSdp {
    let lines = split_lines(text);
    let mut session_connection: Option<String> = None;
    let mut media_sections: Vec<MediaSection> = Vec::new();

    let mut i = 0;
    // Preamble: up to the first m= line.
    while i < lines.len() && !lines[i].starts_with("m=") {
        let line = lines[i];
        if line.starts_with("c=") && session_connection.is_none() {
            session_connection = Some(line.to_string());
        }
        i += 1;
    }

    while i < lines.len() {
        let m_line = lines[i];
        i += 1;
        let parts = ws_tokens(&m_line[2..]);
        let media = parts.first().copied().unwrap_or("").to_string();
        let port = parts.get(1).and_then(|p| parse_int_js(p)).unwrap_or(0);
        let proto = parts.get(2).copied().unwrap_or("RTP/AVP").to_string();
        let payload_types: Vec<i64> = parts
            .iter()
            .skip(3)
            .filter_map(|f| parse_int_js(f))
            .collect();

        let mut connection: Option<String> = None;
        let mut rtpmaps: BTreeMap<i64, String> = BTreeMap::new();
        let mut fmtps: BTreeMap<i64, String> = BTreeMap::new();
        let mut direction: Option<String> = None;

        while i < lines.len() && !lines[i].starts_with("m=") {
            let line = lines[i];
            if line.starts_with("c=") && connection.is_none() {
                connection = Some(line.to_string());
            } else if let Some(rest) = line.strip_prefix("a=rtpmap:") {
                let rest = rest.trim();
                if let Some(space) = rest.find(' ') {
                    if let Some(pt) = parse_int_js(&rest[..space]) {
                        let codec = rest[space + 1..].trim();
                        if !codec.is_empty() {
                            rtpmaps.insert(pt, codec.to_string());
                        }
                    }
                }
            } else if let Some(rest) = line.strip_prefix("a=fmtp:") {
                let rest = rest.trim();
                if let Some(space) = rest.find(' ') {
                    if let Some(pt) = parse_int_js(&rest[..space]) {
                        fmtps.insert(pt, line.to_string());
                    }
                }
            } else if matches!(line, "a=sendrecv" | "a=sendonly" | "a=recvonly" | "a=inactive") {
                direction = Some(line[2..].to_string());
            }
            i += 1;
        }

        media_sections.push(MediaSection {
            media,
            port,
            proto,
            payload_types,
            connection,
            rtpmaps,
            fmtps,
            direction,
        });
    }

    ParsedSdp { session_connection, media_sections }
}

fn codec_key(pt: i64, rtpmaps: &BTreeMap<i64, String>) -> Option<String> {
    if let Some(v) = rtpmaps.get(&pt) {
        return Some(v.to_lowercase());
    }
    static_pt(pt).map(|s| s.to_lowercase())
}

struct Intersected {
    bob_pts: Vec<i64>,
}

/// `None` ≙ empty intersection.
fn intersect_codecs(bob: &MediaSection, alice: &MediaSection) -> Option<Intersected> {
    let mut alice_keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for &pt in &alice.payload_types {
        if let Some(key) = codec_key(pt, &alice.rtpmaps) {
            alice_keys.insert(key);
        }
    }
    let mut matched: Vec<i64> = Vec::new();
    for &pt in &bob.payload_types {
        let Some(key) = codec_key(pt, &bob.rtpmaps) else { continue };
        if alice_keys.contains(&key) {
            matched.push(pt);
        }
    }
    if matched.is_empty() {
        None
    } else {
        Some(Intersected { bob_pts: matched })
    }
}

fn build_answer_section(
    bob: &MediaSection,
    alice: Option<&MediaSection>,
    alice_session_connection: Option<&str>,
    intersected: Option<&Intersected>,
    extra_offer_attrs: &[String],
) -> String {
    let mut lines: Vec<String> = Vec::new();

    if let (Some(inter), Some(alice)) = (intersected, alice) {
        lines.push(format!(
            "m={} {} {} {}",
            bob.media,
            alice.port,
            bob.proto,
            inter.bob_pts.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(" ")
        ));
        let c = alice.connection.as_deref().or(alice_session_connection);
        if let Some(c) = c {
            lines.push(c.to_string());
        }
        for &pt in &inter.bob_pts {
            if let Some(rtpmap) = bob.rtpmaps.get(&pt) {
                lines.push(format!("a=rtpmap:{pt} {rtpmap}"));
            }
            if let Some(fmtp) = bob.fmtps.get(&pt) {
                lines.push(fmtp.clone());
            }
        }
        for attr in extra_offer_attrs {
            lines.push(attr.clone());
        }
        lines.push(match &bob.direction {
            Some(d) => format!("a={d}"),
            None => "a=sendrecv".to_string(),
        });
    } else {
        // No matching Alice m-section: disabled placeholder (RFC 3264 §6).
        match bob.payload_types.first() {
            None => lines.push(format!("m={} 0 {} 0", bob.media, bob.proto)),
            Some(&fallback_pt) => {
                lines.push(format!("m={} 0 {} {}", bob.media, bob.proto, fallback_pt));
                if let Some(rtpmap) = bob.rtpmaps.get(&fallback_pt) {
                    lines.push(format!("a=rtpmap:{fallback_pt} {rtpmap}"));
                }
            }
        }
        for attr in extra_offer_attrs {
            lines.push(attr.clone());
        }
        lines.push("a=inactive".to_string());
    }

    lines.join(CRLF)
}

/// Session-level `a=x-offer-id:` attributes the answer must echo for the test
/// harness's offer/answer correlation.
fn extract_echo_attrs(bob_offer_text: &str) -> Vec<String> {
    split_lines(bob_offer_text)
        .into_iter()
        .filter(|l| l.starts_with("a=x-offer-id:"))
        .map(|l| l.to_string())
        .collect()
}

/// Build an answer to `bob_offer` whose addresses/ports come from `alice_offer`.
/// `alice_offer == None` (or empty) yields [`SdpBuildResult::NoAliceSdp`].
pub fn build_answer_from_offer(
    bob_offer: &[u8],
    alice_offer: Option<&[u8]>,
    options: &BuildAnswerOptions,
) -> SdpBuildResult {
    let Some(alice_bytes) = alice_offer else {
        return SdpBuildResult::NoAliceSdp;
    };
    let alice_text = String::from_utf8_lossy(alice_bytes);
    if alice_text.is_empty() {
        return SdpBuildResult::NoAliceSdp;
    }

    let bob_text = String::from_utf8_lossy(bob_offer);
    let bob = parse_sdp(&bob_text);
    let alice = parse_sdp(&alice_text);

    if bob.media_sections.is_empty() {
        return SdpBuildResult::NoAliceSdp;
    }
    if alice.media_sections.is_empty() {
        return SdpBuildResult::NoAliceSdp;
    }

    let echo_attrs = extract_echo_attrs(&bob_text);

    let mut sections: Vec<String> = Vec::new();
    for (idx, bob_section) in bob.media_sections.iter().enumerate() {
        match alice.media_sections.get(idx) {
            Some(alice_section) => {
                let Some(intersected) = intersect_codecs(bob_section, alice_section) else {
                    return SdpBuildResult::NoCommonCodec { m_line_index: idx };
                };
                sections.push(build_answer_section(
                    bob_section,
                    Some(alice_section),
                    alice.session_connection.as_deref(),
                    Some(&intersected),
                    &echo_attrs,
                ));
            }
            None => {
                sections.push(build_answer_section(
                    bob_section,
                    None,
                    alice.session_connection.as_deref(),
                    None,
                    &echo_attrs,
                ));
            }
        }
    }

    let origin_ip = sdp_origin_address(&options.local_ip);
    let sess_id = sdp_session_id(options.now_ms);
    let session_lines = [
        "v=0".to_string(),
        format!("o=b2bua {sess_id} {sess_id} IN IP4 {origin_ip}"),
        "s=-".to_string(),
        "t=0 0".to_string(),
    ];

    let body = format!(
        "{}{CRLF}{}{CRLF}",
        session_lines.join(CRLF),
        sections.join(CRLF)
    );
    SdpBuildResult::Ok(body.into_bytes())
}

/// Normalise an SDP-origin address: `0.0.0.0`/`::` → `127.0.0.1`.
pub fn sdp_origin_address(local_ip: &str) -> String {
    if local_ip == "0.0.0.0" || local_ip == "::" {
        "127.0.0.1".to_string()
    } else {
        local_ip.to_string()
    }
}

/// Derive a non-zero `o=` session id from a wall-clock reading (epoch-seconds).
pub fn sdp_session_id(now_ms: i64) -> i64 {
    let sec = now_ms.div_euclid(1000);
    if sec > 0 {
        sec
    } else {
        1
    }
}

// ===========================================================================
// Replay-owned field rewrite — connection addresses and media ports
// ===========================================================================

/// Rewrite an SDP body's replay-owned fields — connection addresses to `addr`
/// where one is given, media ports to what `port_of` books — keeping every
/// other byte of the body, line endings included, exactly as stored.
///
/// `port_of` is asked once per ACTIVE `m=` line, in body order, with the line's
/// port-pair count (RFC 4566 §5.14); it returns the replacement port, or `None`
/// to leave the line as stored. A port-0 stream is one the description rejected
/// or disabled (RFC 3264 §5.1): it stays zero and is never offered for booking,
/// so a caller indexing streams by booking skips it. A line the grammar does
/// not fit rides verbatim rather than being guessed at.
pub fn rewrite_connection_and_ports(
    sdp: &str,
    addr: Option<&str>,
    mut port_of: impl FnMut(u16) -> Option<u16>,
) -> String {
    let mut out = String::with_capacity(sdp.len());
    for line in sdp.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if let (Some(addr), true) = (addr, trimmed.starts_with("c=IN IP4 ")) {
            out.push_str("c=IN IP4 ");
            out.push_str(addr);
        } else if trimmed.starts_with("m=") {
            out.push_str(&rewrite_media_line(trimmed, &mut port_of));
        } else {
            out.push_str(trimmed);
        }
        if line.ends_with("\r\n") {
            out.push_str("\r\n");
        } else if line.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// One `m=<media> <port>[/<count>] <proto> <fmt>…` line with the booked port in
/// place of the stored one (RFC 4566 §5.14). Everything else on the line — the
/// media kind, the port-pair count, the transport, the whole format list —
/// rides byte-for-byte.
fn rewrite_media_line(line: &str, port_of: &mut impl FnMut(u16) -> Option<u16>) -> String {
    let Some((kind, tail)) = line["m=".len()..].split_once(' ') else { return line.to_string() };
    let (port_field, rest) = match tail.split_once(' ') {
        Some((port, rest)) => (port, Some(rest)),
        None => (tail, None),
    };
    let (port, pair_count) = match port_field.split_once('/') {
        Some((port, count)) => (port, Some(count)),
        None => (port_field, None),
    };
    let Ok(port) = port.parse::<u16>() else { return line.to_string() };
    if port == 0 {
        return line.to_string();
    }
    let pairs = pair_count.and_then(|count| count.parse::<u16>().ok()).unwrap_or(1);
    let Some(booked) = port_of(pairs) else { return line.to_string() };
    let mut out = format!("m={kind} {booked}");
    if let Some(count) = pair_count {
        out.push('/');
        out.push_str(count);
    }
    if let Some(rest) = rest {
        out.push(' ');
        out.push_str(rest);
    }
    out
}

#[cfg(test)]
mod rewrite_tests {
    use super::rewrite_connection_and_ports;

    /// The grammar contract: only `c=IN IP4` addresses and active `m=` ports
    /// move; the `o=` line's address, attributes, pair counts, format lists and
    /// the mixed line endings all ride byte-for-byte.
    #[test]
    fn only_the_named_fields_move_and_every_other_byte_rides() {
        let sdp = "v=0\r\no=- 1 1 IN IP4 1.2.3.4\r\nc=IN IP4 1.2.3.4\r\n\
                   m=audio 5000/2 RTP/AVP 8 101 0\r\na=rtpmap:101 telephone-event/8000\n\
                   m=image 5008 udptl t38\r\n";
        let mut booked = vec![];
        let out = rewrite_connection_and_ports(sdp, Some("127.0.0.9"), |pairs| {
            booked.push(pairs);
            Some(41000 + 10 * booked.len() as u16)
        });
        assert_eq!(
            out,
            "v=0\r\no=- 1 1 IN IP4 1.2.3.4\r\nc=IN IP4 127.0.0.9\r\n\
             m=audio 41010/2 RTP/AVP 8 101 0\r\na=rtpmap:101 telephone-event/8000\n\
             m=image 41020 udptl t38\r\n"
        );
        assert_eq!(booked, [2, 1], "each active line offers its own pair count");
    }

    /// A port-0 stream is never offered for booking (RFC 3264 §5.1), a `None`
    /// booking leaves its line as stored, and no address means no `c=` rewrite.
    #[test]
    fn rejected_streams_and_declined_bookings_ride_verbatim() {
        let sdp = "c=IN IP4 1.2.3.4\r\nm=audio 0 RTP/AVP 8\r\nm=image 6000 udptl t38\r\n";
        let mut offers = 0;
        let out = rewrite_connection_and_ports(sdp, None, |_| {
            offers += 1;
            None
        });
        assert_eq!(out, sdp);
        assert_eq!(offers, 1, "only the live stream was offered");
    }

    /// A line the m= grammar does not fit is not guessed at.
    #[test]
    fn an_unparsable_media_line_rides_verbatim() {
        let sdp = "m=audio\r\nm=audio five RTP/AVP 0\r\n";
        let out = rewrite_connection_and_ports(sdp, None, |_| Some(41000));
        assert_eq!(out, sdp);
    }
}
