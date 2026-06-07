//! Wire-text rendering + SIP label extraction shared by the renderers.
//!
//! Port of `wireText` / the message-description helpers in
//! `svg-sequence-diagram.ts`. `wire_text` is a straight per-byte transform of
//! the captured packet — duplicate headers, casing, folding and exact
//! whitespace all survive, because the report must show what crossed the wire,
//! not a re-serialization. The label helpers parse `raw` (via `sip-message`)
//! only to derive an arrow caption; a parse failure degrades gracefully to the
//! request/status line.

use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

/// Render bytes exactly as they crossed the wire. Printable ASCII plus
/// CR/LF/HTAB pass through; every other byte becomes `\xNN` so it stays
/// visible and unambiguous.
pub fn wire_text(raw: &[u8]) -> String {
    let mut out = String::with_capacity(raw.len());
    for &b in raw {
        if b == 0x09 || b == 0x0a || b == 0x0d || (0x20..=0x7e).contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("\\x{b:02X}"));
        }
    }
    out
}

/// Parsed facets a renderer needs for one message. All best-effort: an
/// unparseable datagram yields a `raw`-derived label and empty correlation
/// fields, never a panic.
pub struct Facets {
    /// Arrow caption, e.g. `INVITE sip:bob@…` or `200 OK (INVITE)`.
    pub label: String,
    /// Call-ID, for colour-banding flows. Empty when unknown.
    pub call_id: String,
    /// `F:<8> T:<8>` tag hint, mirroring the SVG renderer.
    pub tag_label: String,
    /// `true` for responses (affects nothing structural; kept for parity).
    pub is_response: bool,
}

fn first_line(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    s.lines().next().unwrap_or("").trim_end().to_string()
}

fn has_sdp(msg: &SipMessage) -> bool {
    let body = match msg {
        SipMessage::Request(r) => &r.body,
        SipMessage::Response(r) => &r.body,
    };
    if body.is_empty() {
        return false;
    }
    msg.get_header("content-type")
        .iter()
        .any(|v| v.contains("application/sdp"))
        || body.starts_with(b"v=0")
}

/// Extract the renderer facets from raw packet bytes.
pub fn facets(raw: &[u8]) -> Facets {
    let parser = CustomParser::new();
    let Ok(msg) = parser.parse(raw) else {
        return Facets {
            label: first_line(raw),
            call_id: String::new(),
            tag_label: String::new(),
            is_response: false,
        };
    };

    let sdp_tag = if has_sdp(&msg) { " [SDP]" } else { "" };
    let (label, is_response, from_tag, to_tag, call_id) = match &msg {
        SipMessage::Request(r) => {
            let initial_invite = r.method == "INVITE" && r.to.tag.is_none();
            let label = if initial_invite {
                format!("{} {}{}", r.method, r.uri, sdp_tag)
            } else {
                format!("{}{}", r.method, sdp_tag)
            };
            (
                label,
                false,
                r.from.tag.clone().unwrap_or_default(),
                r.to.tag.clone().unwrap_or_default(),
                r.call_id.clone(),
            )
        }
        SipMessage::Response(r) => {
            let method_tag = if r.cseq.method.as_str().is_empty() {
                String::new()
            } else {
                format!(" ({})", r.cseq.method)
            };
            (
                format!("{} {}{}{}", r.status, r.reason, method_tag, sdp_tag),
                true,
                r.from.tag.clone().unwrap_or_default(),
                r.to.tag.clone().unwrap_or_default(),
                r.call_id.clone(),
            )
        }
    };

    let mut parts = Vec::new();
    if !from_tag.is_empty() {
        parts.push(format!("F:{}", &from_tag[..from_tag.len().min(8)]));
    }
    if !to_tag.is_empty() {
        parts.push(format!("T:{}", &to_tag[..to_tag.len().min(8)]));
    }

    Facets {
        label,
        call_id,
        tag_label: parts.join(" "),
        is_response,
    }
}

/// Format a virtual-clock offset (ms, relative to the first entry) as
/// `T+SEC.mmms` — e.g. `T+0.015s`, `T+1m02.345s`. Port of
/// `formatRelativeTimestamp`.
pub fn format_relative(ms: i64) -> String {
    let ms = ms.max(0);
    let total_sec = ms / 1000;
    let millis = ms % 1000;
    let min = total_sec / 60;
    let sec = total_sec % 60;
    if min > 0 {
        format!("T+{min}m{sec:02}.{millis:03}s")
    } else {
        format!("T+{sec}.{millis:03}s")
    }
}

/// `format_relative` without the `T+` prefix, for the text report's
/// `[T+…]` blocks.
pub fn format_clock(ms: i64) -> String {
    format_relative(ms).trim_start_matches("T+").to_string()
}
