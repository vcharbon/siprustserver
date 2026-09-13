//! The SDP session description read as a DOCUMENT: the line walk, the
//! session/media split, and the field extractions a consumer keys on.
//!
//! This is the only home for SDP document grammar, the way [`crate::sniff`] is
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

/// One `m=`-rooted media description with its trailing media-level lines. The
/// m-line shape is `m=<type> <port> <transport> <fmt> ...`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaLine {
    /// Media type token (e.g. `audio`, `video`, `application`).
    pub r#type: String,
    /// The m-line's port. `None` where no digits read there; `Some(0)` is a
    /// stream REJECTED (RFC 3264 §6), which is a statement, not an absence.
    pub port: Option<i64>,
    /// Transport token (e.g. `RTP/AVP`, `RTP/SAVP`, `UDP/TLS/RTP/SAVPF`).
    pub transport: String,
    /// Format / payload-type tokens that follow `<port> <transport>`.
    pub formats: Vec<String>,
    /// Every `a=...` line inside this media block, without the leading `a=`.
    pub attributes: Vec<String>,
    /// The first `c=` line inside this media block, which overrides the
    /// session-level one for this stream.
    pub c_line: Option<String>,
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
    /// The media blocks in document order.
    pub media: Vec<MediaLine>,
    /// The decoded text, for byte-level comparisons.
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

/// The `o=<username> <sess-id> <sess-version> <nettype> <addrtype> <address>`
/// line lifted into its six fields (RFC 4566 §5.2), plus the description's
/// other lines so a consumer can ask whether anything BUT the origin changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SdpOrigin {
    pub username: String,
    pub session_id: String,
    pub session_version: u64,
    pub nettype: String,
    pub addrtype: String,
    pub unicast_address: String,
    /// The `o=` line as the wire spelled it, leading `o=` included.
    pub raw_origin_line: String,
    /// The description's lines with the `o=` line removed, newline-joined —
    /// what an "everything but the origin changed" comparison reads.
    pub body_excluding_origin: String,
}

/// Leading-digit integer read: an optional sign, then digits, stopping at the
/// first byte that is not one. `None` where no digit follows the sign.
fn leading_int(s: &str) -> Option<i64> {
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

/// Case-insensitive prefix strip; `prefix` is lowercase ASCII.
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

/// A media block under construction, finalised into [`MediaLine`].
struct MutableMedia {
    r#type: String,
    port: Option<i64>,
    transport: String,
    formats: Vec<String>,
    attributes: Vec<String>,
    c_line: Option<String>,
    ptime: Option<i64>,
}

/// Start a media block from an `m=` line's value (no leading `m=`).
fn new_media(raw_m_line: &str) -> MutableMedia {
    let tokens: Vec<&str> = raw_m_line.split_whitespace().collect();
    MutableMedia {
        r#type: tokens.first().copied().unwrap_or("").to_string(),
        port: tokens.get(1).and_then(|p| leading_int(p)),
        transport: tokens.get(2).copied().unwrap_or("").to_string(),
        formats: tokens.iter().skip(3).map(|t| t.to_string()).collect(),
        attributes: Vec::new(),
        c_line: None,
        ptime: None,
    }
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
    let text = String::from_utf8_lossy(body).into_owned();
    if !text.starts_with("v=") {
        return None;
    }
    let lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));

    let mut version: Option<String> = None;
    let mut origin: Option<String> = None;
    let mut session_name: Option<String> = None;
    let mut t_line: Option<String> = None;
    let mut session_c_line: Option<String> = None;
    let mut media: Vec<MutableMedia> = Vec::new();

    for line in lines {
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

        match key {
            b'c' if current.c_line.is_none() => current.c_line = Some(value.to_string()),
            b'a' => {
                current.attributes.push(value.to_string());
                if current.ptime.is_none() {
                    if let Some(rest) = strip_prefix_ci(value, "ptime:") {
                        current.ptime = leading_int(rest.trim());
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

/// The tokens after `<type> <port> <transport>` on an `m=` line's value — the
/// format / payload-type list. Empty where the line carries none.
pub fn extract_format_list(m_line: &str) -> Vec<String> {
    let tokens: Vec<&str> = m_line.split_whitespace().collect();
    if tokens.len() > 3 {
        tokens[3..].iter().map(|t| t.to_string()).collect()
    } else {
        Vec::new()
    }
}

/// The block's direction attribute; absence is [`SdpDirection::SendRecv`]
/// (RFC 3264 §6.1).
pub fn extract_direction(media: &MediaLine) -> SdpDirection {
    for attr in &media.attributes {
        match attr.trim().to_ascii_lowercase().as_str() {
            "sendrecv" => return SdpDirection::SendRecv,
            "sendonly" => return SdpDirection::SendOnly,
            "recvonly" => return SdpDirection::RecvOnly,
            "inactive" => return SdpDirection::Inactive,
            _ => {}
        }
    }
    SdpDirection::SendRecv
}

/// Every `a=rtpmap:<pt> <encoding>[/<rate>[/<channels>]]` in `media` as ordered
/// `(payload_type, encoding)` pairs, the encoding kept verbatim. The FIRST
/// occurrence of a payload type wins — a repeated binding states nothing new.
pub fn extract_rtpmaps(media: &MediaLine) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for attr in &media.attributes {
        let Some(tail) = strip_prefix_ci(attr, "rtpmap:") else { continue };
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

/// The `o=` line of a session description, or `None` where the body is not a
/// `v=0` description, carries no `o=` line, or spells one with fewer than the
/// six fields §5.2 defines / an unreadable sess-version.
pub fn parse_origin(body: &[u8]) -> Option<SdpOrigin> {
    if body.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(body);
    if !text.starts_with("v=0") {
        return None;
    }
    let lines: Vec<&str> = text.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    let o_idx = lines.iter().position(|l| l.starts_with("o="))?;
    let o_line = lines[o_idx];
    let parts: Vec<&str> = o_line[2..].split_whitespace().collect();
    if parts.len() < 6 {
        return None;
    }
    let session_version = parts[2].parse::<u64>().ok()?;
    let others = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != o_idx)
        .map(|(_, l)| *l)
        .collect::<Vec<_>>()
        .join("\n");
    Some(SdpOrigin {
        username: parts[0].to_string(),
        session_id: parts[1].to_string(),
        session_version,
        nettype: parts[3].to_string(),
        addrtype: parts[4].to_string(),
        unicast_address: parts[5].to_string(),
        raw_origin_line: o_line.to_string(),
        body_excluding_origin: others,
    })
}

impl SdpOrigin {
    /// The five-field identity `o=` states about the SESSION — everything but
    /// the version, which is the revision counter (RFC 4566 §5.2). Two
    /// descriptions of one session agree on all five.
    pub fn identifies_same_session(&self, other: &SdpOrigin) -> bool {
        self.username == other.username
            && self.session_id == other.session_id
            && self.nettype == other.nettype
            && self.addrtype == other.addrtype
            && self.unicast_address == other.unicast_address
    }

    /// The `o=` line a re-offer of THIS session states (RFC 3264 §8): the five
    /// identity fields unchanged, the version one above this one.
    pub fn next_version_line(&self) -> String {
        format!(
            "o={} {} {} {} {} {}",
            self.username,
            self.session_id,
            self.session_version + 1,
            self.nettype,
            self.addrtype,
            self.unicast_address
        )
    }
}

/// `new_offer` re-stated as a re-offer of the session `previous` described
/// (RFC 3264 §8): its `o=` line is replaced by `previous`'s with the version
/// incremented by one, every other line kept as written. `None` where either
/// body carries no readable `o=` line — the caller then has no session to
/// continue and sends the offer as it is.
pub fn reoffer_continuing(new_offer: &[u8], previous: &[u8]) -> Option<Vec<u8>> {
    let prior = parse_origin(previous)?;
    let current = parse_origin(new_offer)?;
    let text = String::from_utf8_lossy(new_offer);
    let replaced = text.replacen(&current.raw_origin_line, &prior.next_version_line(), 1);
    Some(replaced.into_bytes())
}

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
        // No media-level c= — the session-level one stands for this stream.
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
        assert_eq!(extract_rtpmaps(video), vec![("96".to_string(), "H264/90000".to_string())]);
    }

    #[test]
    fn an_audio_rtpmap_reads_an_absent_channel_count_as_one() {
        assert_eq!(canonical_rtpmap("audio", "PCMA/8000"), "PCMA/8000/1");
        assert_eq!(canonical_rtpmap("AUDIO", "PCMA/8000"), "PCMA/8000/1");
        assert_eq!(canonical_rtpmap("audio", "PCMA/8000/1"), "PCMA/8000/1");
        assert_eq!(canonical_rtpmap("audio", "opus/48000/2"), "opus/48000/2");
    }

    #[test]
    fn a_non_audio_rtpmap_and_an_unreadable_one_keep_what_was_written() {
        assert_eq!(canonical_rtpmap("video", "H264/90000"), "H264/90000");
        assert_eq!(canonical_rtpmap("application", "H224/4800"), "H224/4800");
        assert_eq!(canonical_rtpmap("audio", "PCMA"), "PCMA");
        assert_eq!(canonical_rtpmap("audio", "PCMA/"), "PCMA/");
        assert_eq!(canonical_rtpmap("audio", "/8000"), "/8000");
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
        assert_eq!(doc.media[0].port, Some(0), "rejected stream carries port 0");
        assert_eq!(extract_direction(&doc.media[0]), SdpDirection::Inactive);
    }

    #[test]
    fn non_sdp_body_yields_none() {
        assert!(parse_sdp_body(b"").is_none());
        assert!(parse_sdp_body(b"not sdp at all").is_none());
        assert!(parse_sdp_body(b"o=- 0 0 IN IP4 127.0.0.1\r\n").is_none());
    }

    #[test]
    fn unparseable_port_is_none_not_zero() {
        let body = b"v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=audio xyz RTP/AVP 0\r\n";
        let doc = parse_sdp_body(body).expect("sdp");
        assert_eq!(doc.media[0].port, None, "no digits read as no port, never as zero");
    }

    #[test]
    fn extract_format_list_drops_head_tokens() {
        assert_eq!(extract_format_list("audio 49170 RTP/AVP 0 8 96"), vec!["0", "8", "96"]);
        assert!(extract_format_list("audio 49170 RTP/AVP").is_empty());
        assert!(extract_format_list("audio").is_empty());
    }

    #[test]
    fn origin_lifts_its_six_fields_and_the_rest_of_the_body() {
        let o = parse_origin(AUDIO_OFFER).expect("origin");
        assert_eq!(o.username, "alice");
        assert_eq!(o.session_id, "2890844526");
        assert_eq!(o.session_version, 2890844526);
        assert_eq!(o.nettype, "IN");
        assert_eq!(o.addrtype, "IP4");
        assert_eq!(o.unicast_address, "10.0.0.1");
        assert!(!o.body_excluding_origin.contains("o="));
        assert!(o.body_excluding_origin.contains("m=audio"));
    }

    #[test]
    fn origin_rejects_what_is_not_a_v0_description() {
        assert!(parse_origin(b"").is_none());
        assert!(parse_origin(b"not sdp").is_none());
        assert!(parse_origin(b"v=0\r\ns=-\r\n").is_none(), "no o= line");
        assert!(parse_origin(b"v=0\r\no=alice 1 1 IN\r\n").is_none(), "fewer than six fields");
        assert!(parse_origin(b"v=0\r\no=alice 1 x IN IP4 1.2.3.4\r\n").is_none(), "no version");
    }

    #[test]
    fn same_session_is_the_five_fields_but_the_version() {
        let a = parse_origin(b"v=0\r\no=alice 1 1 IN IP4 10.0.0.1\r\n").expect("a");
        let bumped = parse_origin(b"v=0\r\no=alice 1 9 IN IP4 10.0.0.1\r\n").expect("b");
        let moved = parse_origin(b"v=0\r\no=alice 1 1 IN IP4 10.0.0.2\r\n").expect("c");
        assert!(a.identifies_same_session(&bumped), "only the version rose");
        assert!(!a.identifies_same_session(&moved), "the address is part of the identity");
    }

    /// RFC 3264 §8: a re-offer keeps the previous description's `o=` identity
    /// and increments only its version; the new offer's other lines ride as
    /// written.
    #[test]
    fn a_continuing_reoffer_carries_the_previous_origin_one_version_up() {
        let previous = b"v=0\r\no=- 56623135 20000000 IN IP4 10.1.1.1\r\ns=-\r\nc=IN IP4 10.1.1.1\r\nt=0 0\r\nm=audio 38278 RTP/AVP 8\r\n";
        let new_offer = b"v=0\r\no=- 3997628865 3997628865 IN IP4 10.2.2.2\r\ns=oms\r\nc=IN IP4 10.2.2.2\r\nt=0 0\r\nm=audio 30550 RTP/AVP 9 8\r\na=sendrecv\r\n";
        let out = reoffer_continuing(new_offer, previous).expect("both carry o=");
        assert_eq!(
            out,
            b"v=0\r\no=- 56623135 20000001 IN IP4 10.1.1.1\r\ns=oms\r\nc=IN IP4 10.2.2.2\r\nt=0 0\r\nm=audio 30550 RTP/AVP 9 8\r\na=sendrecv\r\n".to_vec()
        );
        assert!(reoffer_continuing(new_offer, b"v=0\r\ns=-\r\n").is_none(), "no previous origin");
        assert!(reoffer_continuing(b"", previous).is_none(), "no offer");
    }
}
