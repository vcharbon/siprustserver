//! Minimal SDP helpers for the REFER / blind-transfer and hold flows.
//!
//! Deliberately string-based — no external SDP library, not a full parser:
//! only the fields the B2BUA needs. Two tolerance levels coexist:
//!   - [`extract_codec_profile`] degrades gracefully (the core transfer path
//!     prefers this mid-call), while
//!   - [`validate_sdp_body`] enforces the RFC 4566 §5 minimum grammar for
//!     callers that must refuse a malformed body.

const CRLF: &str = "\r\n";

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Split into non-empty lines, tolerating CRLF or LF endings.
pub(crate) fn split_lines(text: &str) -> Vec<&str> {
    text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).filter(|l| !l.is_empty()).collect()
}

/// A leading integer: optional sign, leading digits, stop at the first
/// non-digit; `None` when no digits are present.
fn parse_int_js(s: &str) -> Option<i64> {
    let s = s.trim_start();
    let (neg, unsigned) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let digits = unsigned.get(..unsigned.bytes().take_while(u8::is_ascii_digit).count())?;
    if digits.is_empty() {
        return None;
    }
    let n: i64 = digits.parse().ok()?;
    Some(if neg { -n } else { n })
}

/// The first line of `lines` that starts with `prefix`, and its value after
/// the prefix.
fn line_with<'a>(lines: &[&'a str], prefix: &str) -> Option<(&'a str, &'a str)> {
    lines.iter().find_map(|l| l.strip_prefix(prefix).map(|value| (*l, value)))
}

/// Split a value on runs of whitespace, dropping empties.
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
    let Some((v_line, _)) = line_with(&lines, "v=") else {
        return Err(SdpValidationError::new("missing v= line"));
    };
    if v_line != "v=0" {
        return Err(SdpValidationError::new(format!("non-zero protocol-version: \"{v_line}\"")));
    }

    // o= must have exactly six SP-tokens.
    let Some((o_line, o_value)) = line_with(&lines, "o=") else {
        return Err(SdpValidationError::new("missing o= line"));
    };
    let o_fields = ws_tokens(o_value);
    if o_fields.len() != 6 {
        return Err(SdpValidationError::new(format!(
            "o= line has {} tokens (want 6): \"{o_line}\"",
            o_fields.len()
        )));
    }

    // Every m= line has ≥ 4 tokens (<media> <port> <proto> <fmt>+).
    for line in &lines {
        let Some(m_value) = line.strip_prefix("m=") else {
            continue;
        };
        let tokens = ws_tokens(m_value);
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
    let v_line = match v_lines.as_slice() {
        [] => return Err(SdpValidationError::new("missing v= line")),
        [one] => **one,
        many => {
            return Err(SdpValidationError::new(format!(
                "{} session descriptions (v= lines) — exactly one required",
                many.len()
            )))
        }
    };
    if v_line != "v=0" {
        return Err(SdpValidationError::new(format!(
            "unexpected v= value '{v_line}' (expected v=0)"
        )));
    }

    validate_sdp_body(body)?;

    // o= sess-id / sess-version bounds. The six-token o= line is there:
    // `validate_sdp_body` has just confirmed it.
    let Some((_, o_value)) = line_with(&lines, "o=") else {
        return Err(SdpValidationError::new("missing o= line"));
    };
    let o_tokens = ws_tokens(o_value);
    let [_, sess_id, sess_version, ..] = o_tokens.as_slice() else {
        return Err(SdpValidationError::new("o= line has fewer than 3 tokens"));
    };
    let fields = [("sess-id", *sess_id), ("sess-version", *sess_version)];
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
            let [media_name, port_tok, _, ..] = m_tokens.as_slice() else {
                return Err(SdpValidationError::new(format!(
                    "m= line '{line}' has fewer than 3 tokens (expected: media port proto fmt...)"
                )));
            };
            if port_tok.is_empty() || !port_tok.bytes().all(|b| b.is_ascii_digit()) {
                return Err(SdpValidationError::new(format!(
                    "m= line port '{port_tok}' is not a non-negative integer"
                )));
            }
            in_media = true;
            current_media_has_c = false;
            current_media_name = media_name.to_string();
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
            let positive_integer = !raw.is_empty()
                && raw.bytes().all(|b| b.is_ascii_digit())
                && raw.bytes().any(|b| b != b'0');
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
    let [nettype, addrtype, address, ..] = toks.as_slice() else {
        return false;
    };
    if !nettype.eq_ignore_ascii_case("IN") {
        return false;
    }
    let addr = address.split('/').next().unwrap_or_default();
    (addrtype.eq_ignore_ascii_case("IP4") && addr == "0.0.0.0")
        || (addrtype.eq_ignore_ascii_case("IP6") && ip6_is_unspecified(addr))
}

/// True iff `addr` spells the IPv6 unspecified address. RFC 4291 §2.2 compresses
/// any run of zero groups to `::`, so `::` and `0:0:0:0:0:0:0:0` are one address:
/// every group reads zero and nothing else is written.
fn ip6_is_unspecified(addr: &str) -> bool {
    addr.contains(':') && addr.bytes().all(|b| b == b':' || b == b'0')
}

// ===========================================================================
// Codec profile extraction + held SDP construction
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
        let Some(m_value) = line.strip_prefix("m=") else {
            continue;
        };
        let parts = ws_tokens(m_value);
        let [kind, _, _, formats @ ..] = parts.as_slice() else {
            continue;
        };
        if *kind != "audio" || formats.is_empty() {
            continue;
        }
        let pts: Vec<i64> = formats.iter().filter_map(|f| parse_int_js(f)).collect();
        if pts.is_empty() {
            continue;
        }
        media = kind.to_string();
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

    for line in lines.iter().skip(start + 1) {
        if line.starts_with("m=") {
            break;
        }
        if !line.starts_with("a=") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            if let Some((pt, _)) = rest.trim().split_once(' ') {
                if let Some(pt) = parse_int_js(pt) {
                    if allowed.contains(&pt) {
                        rtpmaps.push((*line).to_string());
                    }
                }
            }
        } else if let Some(rest) = line.strip_prefix("a=fmtp:") {
            if let Some((pt, _)) = rest.trim().split_once(' ') {
                if let Some(pt) = parse_int_js(pt) {
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
pub fn build_held_sdp_from_profile(
    profile: &CodecProfile,
    options: &BuildHeldSdpOptions,
) -> Vec<u8> {
    let origin_ip = sdp_origin_address(&options.local_ip);
    let sess_id = sdp_session_id(options.now_ms);
    let pts = profile.payload_types.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(" ");
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
    let Some((kind, tail)) = line.strip_prefix("m=").and_then(|value| value.split_once(' ')) else {
        return line.to_string();
    };
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
mod rewrite_tests;
