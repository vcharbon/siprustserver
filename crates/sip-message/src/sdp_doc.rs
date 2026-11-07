//! The SDP session description read as a DOCUMENT: the line walk, the
//! session/media split, and the field extractions a consumer keys on.
//!
//! This is the only home for SDP document grammar, the way `crate::sniff` is
//! the only home for raw SIP header extraction — a rule or an audit reads facts
//! off an [`SdpDoc`] and never walks `v=` / `o=` / `m=` / `a=` lines itself.
//! Its sibling [`crate::sdp`] BUILDS descriptions for the B2BUA's transfer and
//! hold paths and enforces the RFC 4566 §5 minimum grammar; this module only
//! reads what is on the wire.
//!
//! **Best-effort, never rejecting.** A body that does not open with `v=` is not
//! a session description and yields `None`; beyond that the walk is permissive,
//! so a malformed description still produces a shape and the CONSUMER decides
//! what is wrong with it. Strict grammar refusal is
//! [`crate::sdp::validate_sdp_body`]'s.
//!
//! Pure and deterministic: no clocks, no randomness, no I/O.

use lookup_map::LookupSet;

/// One `m=`-rooted media description with its trailing media-level lines. The
/// m-line shape is `m=<type> <port> <transport> <fmt> ...`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaLine {
    /// Media type token (e.g. `audio`, `video`, `application`).
    pub r#type: String,
    /// The m-line's port. `None` where no digits read there; `Some(0)` is a
    /// stream REJECTED (RFC 3264 §6), which is a statement, not an absence.
    pub port: Option<i64>,
    /// The `/<number of ports>` the port field states, where it states one
    /// (RFC 4566 §5.14).
    pub port_count: Option<i64>,
    /// Transport token (e.g. `RTP/AVP`, `RTP/SAVP`, `UDP/TLS/RTP/SAVPF`).
    pub transport: String,
    /// Format / payload-type tokens that follow `<port> <transport>`.
    pub formats: Vec<String>,
    /// Every `a=...` line inside this media block, without the leading `a=`.
    pub attributes: Vec<String>,
    /// The first `c=` line inside this media block, which overrides the
    /// session-level one for this stream.
    pub c_line: Option<String>,
    /// Every `b=` line inside this media block, without the leading `b=`;
    /// where there is none the session-level ones apply.
    pub bandwidths: Vec<String>,
    /// The first `k=` line inside this media block, which overrides the
    /// session-level one for this stream.
    pub key: Option<String>,
    /// `a=ptime:<N>` read as a number, where the block carries one.
    pub ptime: Option<i64>,
}

/// A parsed session description: the session-level lines plus the media
/// descriptions in document order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SdpDoc {
    /// The `v=` line's value (`"0"` is the only value RFC 4566 defines).
    pub version: Option<String>,
    /// The `o=` line's value — everything after `o=`.
    pub origin: Option<String>,
    /// The `s=` line's value.
    pub session_name: Option<String>,
    /// The `t=` line's value, `"<start> <stop>"`.
    pub t_line: Option<String>,
    /// The session-level `c=` line's value; a media block may override it.
    pub c_line: Option<String>,
    /// Every session-level `b=` line's value, the default of a media block
    /// stating none.
    pub bandwidths: Vec<String>,
    /// Every session-level `a=` line, without the leading `a=`: the default of
    /// a media block stating none of the same name (RFC 4566 §5).
    pub attributes: Vec<String>,
    /// The session-level `k=` line's value; a media block may override it.
    pub key: Option<String>,
    /// The media blocks in document order.
    pub media: Vec<MediaLine>,
    /// The text, each byte that is no UTF-8 as the character standing for it
    /// (`sdp_octets`), so two descriptions differing in such a byte differ.
    pub raw: String,
}

/// A media block's direction attribute (RFC 3264 §6.1). Absence IS
/// [`SdpDirection::SendRecv`] — the default the RFC states, not a guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SdpDirection {
    SendRecv,
    SendOnly,
    RecvOnly,
    Inactive,
}

impl SdpDirection {
    /// The lower-case attribute token the wire spells this direction with.
    pub fn token(self) -> &'static str {
        match self {
            SdpDirection::SendRecv => "sendrecv",
            SdpDirection::SendOnly => "sendonly",
            SdpDirection::RecvOnly => "recvonly",
            SdpDirection::Inactive => "inactive",
        }
    }
}

/// Leading-digit integer read: an optional sign, then digits, stopping at the
/// first byte that is not one. `None` where no digit follows the sign.
fn leading_int(s: &str) -> Option<i64> {
    let s = crate::whitespace::trim_start(s);
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

/// Case-insensitive prefix strip; `prefix` is lowercase ASCII.
fn strip_prefix_ci<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let head = value.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        value.get(prefix.len()..)
    } else {
        None
    }
}

/// A description line split into its type letter and its value: `None` where
/// the line is not `<letter>=<value>`.
fn typed_line(line: &str) -> Option<(u8, &str)> {
    let mut chars = line.char_indices();
    let (_, key) = chars.next()?;
    let (eq, '=') = chars.next()? else {
        return None;
    };
    let key = u8::try_from(key).ok().filter(u8::is_ascii)?;
    Some((key, line.get(eq + 1..)?))
}

/// A media block under construction, finalised into [`MediaLine`].
struct MutableMedia {
    r#type: String,
    port: Option<i64>,
    port_count: Option<i64>,
    transport: String,
    formats: Vec<String>,
    attributes: Vec<String>,
    c_line: Option<String>,
    bandwidths: Vec<String>,
    key: Option<String>,
    ptime: Option<i64>,
}

impl MutableMedia {
    fn finished(self) -> MediaLine {
        MediaLine {
            r#type: self.r#type,
            port: self.port,
            port_count: self.port_count,
            transport: self.transport,
            formats: self.formats,
            attributes: self.attributes,
            c_line: self.c_line,
            bandwidths: self.bandwidths,
            key: self.key,
            ptime: self.ptime,
        }
    }
}

/// Start a media block from an `m=` line's value (no leading `m=`).
fn new_media(raw_m_line: &str) -> MutableMedia {
    let tokens: Vec<&str> = crate::whitespace::split(raw_m_line).collect();
    MutableMedia {
        r#type: tokens.first().copied().unwrap_or("").to_string(),
        port: tokens.get(1).and_then(|p| leading_int(p)),
        port_count: tokens.get(1).and_then(|p| leading_int(p.split_once('/')?.1)),
        transport: tokens.get(2).copied().unwrap_or("").to_string(),
        formats: tokens.iter().skip(3).map(|t| t.to_string()).collect(),
        attributes: Vec::new(),
        c_line: None,
        bandwidths: Vec::new(),
        key: None,
        ptime: None,
    }
}

/// One `m=` line's value (no leading `m=`) read as a media block with no
/// media-level line: type, port, transport and formats.
pub fn media_line(m_value: &str) -> MediaLine {
    new_media(m_value).finished()
}

/// The description a body carries, or `None` where the bytes do not open with
/// the canonical `v=` token — which is what makes them a session description at
/// all.
///
/// The walk splits at the first `m=` line: every line before it is
/// session-level, every line after belongs to the media block it follows.
/// Session-level lines are read ONCE — a repeated `t=` states nothing new.
pub fn parse_sdp_body(body: &[u8]) -> Option<SdpDoc> {
    if body.is_empty() {
        return None;
    }
    let text = crate::sdp_octets::decode(body)
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
    if !text.starts_with("v=") {
        return None;
    }
    let lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));

    let mut version: Option<String> = None;
    let mut origin: Option<String> = None;
    let mut session_name: Option<String> = None;
    let mut t_line: Option<String> = None;
    let mut session_c_line: Option<String> = None;
    let mut bandwidths: Vec<String> = Vec::new();
    let mut attributes: Vec<String> = Vec::new();
    let mut session_key: Option<String> = None;
    let mut media: Vec<MutableMedia> = Vec::new();

    for line in lines {
        let Some((key, value)) = typed_line(line) else {
            continue;
        };

        if key == b'm' {
            media.push(new_media(value));
            continue;
        }

        let Some(current) = media.last_mut() else {
            match key {
                b'v' if version.is_none() => version = Some(value.to_string()),
                b'o' if origin.is_none() => origin = Some(value.to_string()),
                b's' if session_name.is_none() => session_name = Some(value.to_string()),
                b't' if t_line.is_none() => t_line = Some(value.to_string()),
                b'c' if session_c_line.is_none() => session_c_line = Some(value.to_string()),
                b'b' => bandwidths.push(value.to_string()),
                b'a' => attributes.push(value.to_string()),
                b'k' if session_key.is_none() => session_key = Some(value.to_string()),
                _ => {}
            }
            continue;
        };

        match key {
            b'c' if current.c_line.is_none() => current.c_line = Some(value.to_string()),
            b'b' => current.bandwidths.push(value.to_string()),
            b'k' if current.key.is_none() => current.key = Some(value.to_string()),
            b'a' => {
                current.attributes.push(value.to_string());
                if current.ptime.is_none() {
                    if let Some(rest) = strip_prefix_ci(value, "ptime:") {
                        current.ptime = leading_int(crate::whitespace::trim(rest));
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
        bandwidths,
        attributes,
        key: session_key,
        media: media.into_iter().map(MutableMedia::finished).collect(),
        raw: text,
    })
}

/// The tokens after `<type> <port> <transport>` on an `m=` line's value — the
/// format / payload-type list. Empty where the line carries none.
pub fn extract_format_list(m_line: &str) -> Vec<String> {
    crate::whitespace::split(m_line).skip(3).map(str::to_string).collect()
}

/// The block's direction attribute; absence is [`SdpDirection::SendRecv`]
/// (RFC 3264 §6.1).
pub fn extract_direction(media: &MediaLine) -> SdpDirection {
    media.attributes.iter().find_map(|a| direction_of(a)).unwrap_or(SdpDirection::SendRecv)
}

/// The direction an attribute value (without the leading `a=`) states, or
/// `None` where it is not one of the four direction attributes (RFC 3264 §6.1).
pub fn direction_of(attr: &str) -> Option<SdpDirection> {
    match crate::whitespace::trim(attr).to_ascii_lowercase().as_str() {
        "sendrecv" => Some(SdpDirection::SendRecv),
        "sendonly" => Some(SdpDirection::SendOnly),
        "recvonly" => Some(SdpDirection::RecvOnly),
        "inactive" => Some(SdpDirection::Inactive),
        _ => None,
    }
}

/// Every `a=rtpmap:<pt> <encoding>[/<rate>[/<channels>]]` in `media` as ordered
/// `(payload_type, encoding)` pairs, the encoding kept verbatim. The FIRST
/// occurrence of a payload type wins — a repeated binding states nothing new.
pub fn extract_rtpmaps(media: &MediaLine) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: LookupSet<&str> = LookupSet::new();
    for attr in &media.attributes {
        let Some(tail) = strip_prefix_ci(attr, "rtpmap:") else { continue };
        let Some((pt, enc)) = crate::whitespace::trim(tail).split_once(' ') else { continue };
        let (pt, enc) = (crate::whitespace::trim(pt), crate::whitespace::trim(enc));
        if pt.is_empty() || enc.is_empty() {
            continue;
        }
        if seen.insert(pt) {
            out.push((pt.to_string(), enc.to_string()));
        }
    }
    out
}

/// Every `a=fmtp:<format> <parameters>` in `media` as ordered
/// `(format, parameters)` pairs, the parameters verbatim. The FIRST occurrence
/// of a format wins.
pub fn extract_fmtps(media: &MediaLine) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen: LookupSet<&str> = LookupSet::new();
    for attr in &media.attributes {
        let Some(tail) = strip_prefix_ci(attr, "fmtp:") else { continue };
        let tail = crate::whitespace::trim(tail);
        let (format, params) = tail.split_once(' ').unwrap_or((tail, ""));
        if !format.is_empty() && seen.insert(format) {
            out.push((format.to_string(), crate::whitespace::trim(params).to_string()));
        }
    }
    out
}

/// Every `a=crypto:<tag> <crypto-suite> <key-params> [<session-params>]` in
/// `media` (RFC 4568 §9.1) as ordered `(tag, suite, rest)` triples, `rest`
/// being everything after the suite, verbatim.
pub fn extract_cryptos(media: &MediaLine) -> Vec<(String, String, String)> {
    media
        .attributes
        .iter()
        .filter_map(|attr| {
            let tail = crate::whitespace::trim(strip_prefix_ci(attr, "crypto:")?);
            let (tag, tail) = tail.split_once(' ')?;
            let tail = crate::whitespace::trim_start(tail);
            let (suite, rest) = tail.split_once(' ').unwrap_or((tail, ""));
            Some((tag.to_string(), suite.to_string(), crate::whitespace::trim(rest).to_string()))
        })
        .collect()
}

/// The rtpmap value RFC 4566 §6 makes an encoding EQUAL to, for a comparison
/// that asks whether two descriptions bind a payload type the same way. On an
/// AUDIO stream the third subfield is the channel count and its absence means
/// one channel, so `PCMA/8000` and `PCMA/8000/1` are one binding and both read
/// back as `PCMA/8000/1`. Every other media type keeps the value as written:
/// §6 states that default for audio alone, and the subfield carries codec
/// parameters elsewhere.
pub fn canonical_rtpmap(media_type: &str, encoding: &str) -> String {
    if !media_type.eq_ignore_ascii_case("audio") {
        return encoding.to_string();
    }
    let mut fields = encoding.split('/');
    match (fields.next(), fields.next(), fields.next()) {
        (Some(name), Some(rate), None) if !name.is_empty() && !rate.is_empty() => {
            format!("{name}/{rate}/1")
        }
        _ => encoding.to_string(),
    }
}

/// A description cut at its `m=` lines, byte for byte: the session-level text,
/// then each media section (its `m=` line and every line up to the next one),
/// with line endings as written. The sections are in `parse_sdp_body`'s media
/// order; the values in them are read there.
pub(crate) struct Sections<'a> {
    pub(crate) session: &'a str,
    pub(crate) media: Vec<Section<'a>>,
}

pub(crate) struct Section<'a> {
    pub(crate) text: &'a str,
}

impl<'a> Sections<'a> {
    /// `text` cut at its `m=` lines; `None` only where a cut would split a
    /// character, which a cut at a line start never does.
    pub(crate) fn of(text: &'a str) -> Option<Self> {
        let mut starts: Vec<usize> = Vec::new();
        let mut offset = 0usize;
        for line in text.split_inclusive('\n') {
            if line.starts_with("m=") {
                starts.push(offset);
            }
            offset += line.len();
        }
        let session_end = starts.first().copied().unwrap_or(text.len());
        let ends = starts.iter().skip(1).copied().chain(std::iter::once(text.len()));
        let media = starts
            .iter()
            .zip(ends)
            .map(|(&at, end)| Some(Section { text: text.get(at..end)? }))
            .collect::<Option<Vec<_>>>()?;
        Some(Self { session: text.get(..session_end)?, media })
    }
}

impl Section<'_> {
    /// The `m=` line's value.
    pub(crate) fn value(&self) -> &str {
        let line = self.text.split('\n').next().unwrap_or_default();
        line.strip_suffix('\r').unwrap_or(line).get(2..).unwrap_or_default()
    }
}

mod origin;

pub use origin::{parse_origin, SdpOrigin};

#[cfg(test)]
mod tests;
