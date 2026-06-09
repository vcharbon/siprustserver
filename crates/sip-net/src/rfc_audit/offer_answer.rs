//! Port of `tests/harness/rules/rfc/_offer-answer.ts` — pure SDP body-parsing
//! primitives shared across the RFC 3264 cross-message rules.
//!
//! Each rule walks per-Call-ID message streams looking at SDP bodies; this
//! helper centralises the line-walk, session/media partitioning, and `m=` /
//! `c=` / `a=` extraction so the rules stay focused on the spec invariant they
//! enforce (m-line count match, media-type match, `t=` equality, rejected-stream
//! port=0, direction pairing, …).
//!
//! Best-effort parsing: a body that doesn't look like SDP (empty, or no `v=`
//! prefix) yields `None`. Beyond that, the helper is permissive — a malformed
//! body still produces a parsed shape so the consuming rule can decide what to
//! flag. Strict SDP-grammar enforcement lives in
//! [`sip_message::sdp::validate_sdp_body`] (peer rule), not here.
//!
//! The helper does NOT depend on the [`sip_message::SipMessage`] parser — it
//! works directly on raw body bytes so it can be used wherever a `body: &[u8]`
//! is available (the consuming rules extract `msg.body` and pass it in).
//!
//! Pure / deterministic; no clocks, no randomness, no I/O. The `o=` line is
//! lifted via [`crate::rfc_audit::dialog_model::parse_sdp_origin`].

use crate::rfc_audit::dialog_model::{parse_sdp_origin, ParsedSdpOrigin};

// ---------------------------------------------------------------------------
// Internal line helpers (JS `Number.parseInt` / `split(/\s+/)` semantics)
// ---------------------------------------------------------------------------

/// JS `Number.parseInt(s, 10)` semantics: optional sign, leading digits, stop
/// at the first non-digit; `None` when no digits are present (the `NaN` case).
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

// ---------------------------------------------------------------------------
// Types (TS `MediaLine` / `SdpDoc` / `SdpDirection`)
// ---------------------------------------------------------------------------

/// One `m=`-rooted media description, with its trailing media-level lines.
///
/// Mirrors the TS `MediaLine`. The m-line shape is
/// `m=<type> <port> <transport> <fmt> ...`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaLine {
    /// Media type token (e.g. `audio`, `video`, `application`).
    pub r#type: String,
    /// Numeric port from the m-line. `None` mirrors the TS `NaN` (unparseable
    /// port); `Some(0)` is a *rejected* stream (RFC 3264 §6).
    pub port: Option<i64>,
    /// Transport token (e.g. `RTP/AVP`, `RTP/SAVP`, `UDP/TLS/RTP/SAVPF`).
    pub transport: String,
    /// Format / payload-type tokens that follow `<port> <transport>`.
    pub formats: Vec<String>,
    /// All `a=...` lines inside this media block, without the leading `a=`.
    pub attributes: Vec<String>,
    /// First `c=` line value inside this media block, if any (session `c=` may
    /// be overridden by this).
    pub c_line: Option<String>,
    /// `a=ptime:<N>` value parsed to a number, if present.
    pub ptime: Option<i64>,
}

/// A parsed SDP document: session-level lines plus ordered media descriptions.
///
/// Mirrors the TS `SdpDoc`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SdpDoc {
    /// Raw `v=` line value (`"0"` is the only valid value per RFC 4566).
    pub version: Option<String>,
    /// Raw `o=` line tail (everything after `o=`), if present.
    pub origin: Option<String>,
    /// Raw `s=` line tail, if present.
    pub session_name: Option<String>,
    /// Raw `t=` line tail (`"<start> <stop>"`), if present.
    pub t_line: Option<String>,
    /// Session-level `c=` line tail, if present (media blocks may override).
    pub c_line: Option<String>,
    /// Ordered media blocks in document order.
    pub media: Vec<MediaLine>,
    /// Raw decoded text for callers that want byte-level comparisons.
    pub raw: String,
}

/// Media direction attribute (RFC 3264 §6.1). Absence defaults to `SendRecv`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SdpDirection {
    SendRecv,
    SendOnly,
    RecvOnly,
    Inactive,
}

/// A mutable in-progress media block, finalised into [`MediaLine`].
struct MutableMedia {
    r#type: String,
    port: Option<i64>,
    transport: String,
    formats: Vec<String>,
    attributes: Vec<String>,
    c_line: Option<String>,
    ptime: Option<i64>,
}

/// Start a media block from a raw `m=` line *value* (no leading `m=`).
/// `m=<type> <port> <transport> <fmt> ...`.
fn new_media(raw_m_line: &str) -> MutableMedia {
    let tokens: Vec<&str> = raw_m_line.split_whitespace().collect();
    let r#type = tokens.first().copied().unwrap_or("").to_string();
    let port = tokens.get(1).and_then(|p| parse_int_js(p));
    let transport = tokens.get(2).copied().unwrap_or("").to_string();
    let formats = tokens.iter().skip(3).map(|t| t.to_string()).collect();
    MutableMedia {
        r#type,
        port,
        transport,
        formats,
        attributes: Vec::new(),
        c_line: None,
        ptime: None,
    }
}

// ---------------------------------------------------------------------------
// parse_sdp_body (TS `parseSdpBody`)
// ---------------------------------------------------------------------------

/// Best-effort SDP parse. Returns `None` when the body is empty or does not
/// start with the canonical `v=` token.
///
/// The walk splits the document at the first `m=` line: every line before is
/// session-level; every line after (until the next `m=`) belongs to the current
/// media block.
pub fn parse_sdp_body(body: &[u8]) -> Option<SdpDoc> {
    if body.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(body).into_owned();
    if !text.starts_with("v=") {
        return None;
    }
    // Split on `\r\n` or `\n` (TS `/\r?\n/`).
    let lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));

    let mut version: Option<String> = None;
    let mut origin: Option<String> = None;
    let mut session_name: Option<String> = None;
    let mut t_line: Option<String> = None;
    let mut session_c_line: Option<String> = None;
    let mut media: Vec<MutableMedia> = Vec::new();

    for line in lines {
        if line.is_empty() {
            continue;
        }
        let bytes = line.as_bytes();
        if bytes.len() < 2 || bytes[1] != b'=' {
            continue;
        }
        let key = bytes[0];
        let value = &line[2..];

        if key == b'm' {
            media.push(new_media(value));
            continue;
        }

        let Some(current) = media.last_mut() else {
            // Session-level lines (set-once, mirroring `??=`).
            match key {
                b'v' if version.is_none() => version = Some(value.to_string()),
                b'o' if origin.is_none() => origin = Some(value.to_string()),
                b's' if session_name.is_none() => session_name = Some(value.to_string()),
                b't' if t_line.is_none() => t_line = Some(value.to_string()),
                b'c' if session_c_line.is_none() => session_c_line = Some(value.to_string()),
                _ => {}
            }
            continue;
        };

        // Media-level lines.
        match key {
            b'c' if current.c_line.is_none() => {
                current.c_line = Some(value.to_string());
            }
            b'a' => {
                current.attributes.push(value.to_string());
                if current.ptime.is_none() {
                    if let Some(rest) = strip_prefix_ci(value, "ptime:") {
                        if let Some(n) = parse_int_js(rest.trim()) {
                            current.ptime = Some(n);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Some(SdpDoc {
        version,
        origin,
        session_name,
        t_line,
        c_line: session_c_line,
        media: media
            .into_iter()
            .map(|m| MediaLine {
                r#type: m.r#type,
                port: m.port,
                transport: m.transport,
                formats: m.formats,
                attributes: m.attributes,
                c_line: m.c_line,
                ptime: m.ptime,
            })
            .collect(),
        raw: text,
    })
}

/// Case-insensitive prefix strip (TS `value.toLowerCase().startsWith(...)` +
/// `value.slice(prefix.length)`). `prefix` must already be lowercase ASCII.
fn strip_prefix_ci<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    if value.len() < prefix.len() {
        return None;
    }
    let (head, tail) = value.split_at(prefix.len());
    if head.eq_ignore_ascii_case(prefix) {
        Some(tail)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Field extractors (TS `extractFormatList` / `extractDirection` /
// `extractRtpmaps`)
// ---------------------------------------------------------------------------

/// Given a raw `m=` line *value* (no leading `m=`), return the tokens after
/// `<type> <port> <transport>` — the format / payload-type list. Empty vec on
/// malformed input.
pub fn extract_format_list(m_line: &str) -> Vec<String> {
    let tokens: Vec<&str> = m_line.split_whitespace().collect();
    if tokens.len() > 3 {
        tokens[3..].iter().map(|t| t.to_string()).collect()
    } else {
        Vec::new()
    }
}

/// Inspect a media block's `a=...` attributes for the direction token. Per
/// RFC 3264 §6.1, absence defaults to [`SdpDirection::SendRecv`].
pub fn extract_direction(media: &MediaLine) -> SdpDirection {
    for attr in &media.attributes {
        let t = attr.trim().to_ascii_lowercase();
        match t.as_str() {
            "sendrecv" => return SdpDirection::SendRecv,
            "sendonly" => return SdpDirection::SendOnly,
            "recvonly" => return SdpDirection::RecvOnly,
            "inactive" => return SdpDirection::Inactive,
            _ => {}
        }
    }
    SdpDirection::SendRecv
}

/// Parse every `a=rtpmap:<pt> <encoding>/<rate>[/<channels>]` attribute in
/// `media` into ordered `(payload_type, encoding)` pairs. The encoding string
/// is preserved verbatim (e.g. `"opus/48000/2"`). PTs that appear without a
/// valid `<pt> <encoding>` split are skipped; the first occurrence of a PT
/// wins (mirrors the TS `if (!out.has(pt))`).
pub fn extract_rtpmaps(media: &MediaLine) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for attr in &media.attributes {
        let Some(tail) = strip_prefix_ci(attr, "rtpmap:") else {
            continue;
        };
        let tail = tail.trim();
        let Some(sp) = tail.find(' ') else { continue };
        if sp == 0 {
            continue;
        }
        let pt = tail[..sp].trim();
        let enc = tail[sp + 1..].trim();
        if pt.is_empty() || enc.is_empty() {
            continue;
        }
        if !out.iter().any(|(p, _)| p == pt) {
            out.push((pt.to_string(), enc.to_string()));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// o= convenience (reuse dialog_model::parse_sdp_origin)
// ---------------------------------------------------------------------------

/// Lift the structured `o=` origin from a raw SDP body, reusing the shared
/// [`parse_sdp_origin`] port. `None` on a non-SDP body.
pub fn parse_origin(body: &[u8]) -> Option<ParsedSdpOrigin> {
    parse_sdp_origin(body)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const AUDIO_OFFER: &[u8] = b"v=0\r\n\
o=alice 2890844526 2890844526 IN IP4 10.0.0.1\r\n\
s=-\r\n\
c=IN IP4 10.0.0.1\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0 8 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=ptime:20\r\n\
a=sendrecv\r\n";

    #[test]
    fn single_audio_m_line_is_lifted() {
        let doc = parse_sdp_body(AUDIO_OFFER).expect("sdp");
        assert_eq!(doc.version.as_deref(), Some("0"));
        assert_eq!(doc.origin.as_deref(), Some("alice 2890844526 2890844526 IN IP4 10.0.0.1"));
        assert_eq!(doc.session_name.as_deref(), Some("-"));
        assert_eq!(doc.t_line.as_deref(), Some("0 0"));
        assert_eq!(doc.c_line.as_deref(), Some("IN IP4 10.0.0.1"));
        assert_eq!(doc.media.len(), 1);

        let m = &doc.media[0];
        assert_eq!(m.r#type, "audio");
        assert_eq!(m.port, Some(49170));
        assert_eq!(m.transport, "RTP/AVP");
        assert_eq!(m.formats, vec!["0", "8", "96"]);
        assert_eq!(m.ptime, Some(20));
        assert_eq!(
            m.attributes,
            vec!["rtpmap:0 PCMU/8000", "rtpmap:96 opus/48000/2", "ptime:20", "sendrecv"]
        );

        // No media-level c= → falls through to session c= (the caller's choice).
        assert_eq!(m.c_line, None);

        assert_eq!(extract_direction(m), SdpDirection::SendRecv);
        assert_eq!(
            extract_rtpmaps(m),
            vec![
                ("0".to_string(), "PCMU/8000".to_string()),
                ("96".to_string(), "opus/48000/2".to_string()),
            ]
        );
    }

    #[test]
    fn two_m_line_offer_keeps_order_and_blocks() {
        let body = b"v=0\r\n\
o=bob 1 1 IN IP4 192.0.2.1\r\n\
s=session\r\n\
t=0 0\r\n\
m=audio 5000 RTP/AVP 0\r\n\
c=IN IP4 192.0.2.1\r\n\
a=sendonly\r\n\
m=video 5002 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=recvonly\r\n";
        let doc = parse_sdp_body(body).expect("sdp");
        assert_eq!(doc.c_line, None, "no session-level c=");
        assert_eq!(doc.media.len(), 2);

        let audio = &doc.media[0];
        assert_eq!(audio.r#type, "audio");
        assert_eq!(audio.port, Some(5000));
        assert_eq!(audio.c_line.as_deref(), Some("IN IP4 192.0.2.1"));
        assert_eq!(extract_direction(audio), SdpDirection::SendOnly);

        let video = &doc.media[1];
        assert_eq!(video.r#type, "video");
        assert_eq!(video.port, Some(5002));
        assert_eq!(video.formats, vec!["96"]);
        assert_eq!(video.c_line, None);
        assert_eq!(extract_direction(video), SdpDirection::RecvOnly);
        assert_eq!(
            extract_rtpmaps(video),
            vec![("96".to_string(), "H264/90000".to_string())]
        );
    }

    #[test]
    fn rejected_stream_has_port_zero() {
        let body = b"v=0\r\n\
o=- 0 0 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=audio 0 RTP/AVP 0\r\n\
a=inactive\r\n";
        let doc = parse_sdp_body(body).expect("sdp");
        assert_eq!(doc.media.len(), 1);
        let m = &doc.media[0];
        assert_eq!(m.port, Some(0), "rejected stream carries port 0");
        assert_eq!(extract_direction(m), SdpDirection::Inactive);
    }

    #[test]
    fn non_sdp_body_yields_none() {
        assert!(parse_sdp_body(b"").is_none());
        assert!(parse_sdp_body(b"not sdp at all").is_none());
        // Missing v= prefix → None even if otherwise SDP-looking.
        assert!(parse_sdp_body(b"o=- 0 0 IN IP4 127.0.0.1\r\n").is_none());
    }

    #[test]
    fn unparseable_port_is_none_not_zero() {
        let body = b"v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=audio xyz RTP/AVP 0\r\n";
        let doc = parse_sdp_body(body).expect("sdp");
        assert_eq!(doc.media[0].port, None, "NaN port maps to None");
    }

    #[test]
    fn extract_format_list_drops_head_tokens() {
        assert_eq!(extract_format_list("audio 49170 RTP/AVP 0 8 96"), vec!["0", "8", "96"]);
        assert!(extract_format_list("audio 49170 RTP/AVP").is_empty());
        assert!(extract_format_list("audio").is_empty());
    }

    #[test]
    fn parse_origin_reuses_dialog_model() {
        let o = parse_origin(AUDIO_OFFER).expect("origin");
        assert_eq!(o.username, "alice");
        assert_eq!(o.session_id, "2890844526");
        assert_eq!(o.unicast_address, "10.0.0.1");
    }
}
