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
fn split_lines(text: &str) -> Vec<&str> {
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
