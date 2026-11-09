//! Structured SDP model, parser and builder for the negotiation engine.
//!
//! Handles the subset a plain RTP/AVP audio UA emits and reads: `v= o= s= c=
//! t= m=audio` plus `a=rtpmap` and direction attributes. Round-trippable. A
//! description is read through `sip_message`'s document reader, so a simulated
//! UA reads each field as the stack does.

pub mod negotiator;

pub use negotiator::{
    is_early_media_authorized, NegotiatedMedia, NegotiationState, OfferAnswerEngine,
    ProvisionalSignals, SdpNegotiationError, SdpRule,
};

/// Media stream direction (RFC 3264 §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaDirection {
    SendRecv,
    SendOnly,
    RecvOnly,
    Inactive,
}

impl MediaDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaDirection::SendRecv => "sendrecv",
            MediaDirection::SendOnly => "sendonly",
            MediaDirection::RecvOnly => "recvonly",
            MediaDirection::Inactive => "inactive",
        }
    }

    #[allow(clippy::should_implement_trait)] // intentionally fallible-to-Option, not FromStr
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "sendrecv" => Some(MediaDirection::SendRecv),
            "sendonly" => Some(MediaDirection::SendOnly),
            "recvonly" => Some(MediaDirection::RecvOnly),
            "inactive" => Some(MediaDirection::Inactive),
            _ => None,
        }
    }

    /// Reverse a direction for the answering side (RFC 3264 §6.1).
    pub fn reverse(self) -> Self {
        match self {
            MediaDirection::SendOnly => MediaDirection::RecvOnly,
            MediaDirection::RecvOnly => MediaDirection::SendOnly,
            // sendrecv / inactive are self-reverse.
            other => other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdpRtpMap {
    pub payload_type: u8,
    pub encoding_name: String,
    pub clock_rate: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdpOrigin {
    pub username: String,
    pub session_id: String,
    pub version: String,
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdpMedia {
    pub kind: String, // "audio", "video", ...
    pub port: u16,
    pub protocol: String,
    pub formats: Vec<u8>,
    pub rtpmap: Vec<SdpRtpMap>,
    pub direction: MediaDirection,
    pub connection_addr: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sdp {
    pub origin: SdpOrigin,
    pub connection_addr: Option<String>,
    pub media: Vec<SdpMedia>,
}

impl Sdp {
    /// The first audio media description, if any.
    pub fn audio_media(&self) -> Option<&SdpMedia> {
        self.media.iter().find(|m| m.kind == "audio")
    }
}

/// The effective connection address for a media description (media-level wins).
pub fn media_connection_addr(sdp: &Sdp, m: &SdpMedia) -> String {
    m.connection_addr
        .clone()
        .or_else(|| sdp.connection_addr.clone())
        .unwrap_or_else(|| sdp.origin.address.clone())
}

/// Parse SDP text through `sip_message`'s document reader. Infallible: text
/// that is no description reads as one with no media, missing fields take RFC
/// defaults, and all conformance checks live in the negotiator. A stream's
/// direction is its own, else the session's (RFC 4566 §5); a port field
/// `<port>/<number of ports>` reads its port (RFC 8866 §5.14).
pub fn parse_sdp(input: &str) -> Sdp {
    let Some(doc) = sip_message::parse_sdp_body(input.as_bytes()) else {
        return Sdp { origin: default_origin(), connection_addr: None, media: Vec::new() };
    };
    let origin = doc.origin.as_deref().map_or_else(default_origin, |value| {
        // o=<user> <sess-id> <sess-version> IN IP4 <addr>
        let p: Vec<&str> = value.split_whitespace().collect();
        SdpOrigin {
            username: p.first().copied().unwrap_or("-").to_string(),
            session_id: p.get(1).copied().unwrap_or("0").to_string(),
            version: p.get(2).copied().unwrap_or("0").to_string(),
            address: p.get(5).copied().unwrap_or("0.0.0.0").to_string(),
        }
    });
    let media = doc.media.iter().map(|m| media_of(&doc, m)).collect();
    Sdp { origin, connection_addr: doc.c_line.as_deref().and_then(c_address), media }
}

fn default_origin() -> SdpOrigin {
    SdpOrigin {
        username: "-".into(),
        session_id: "0".into(),
        version: "0".into(),
        address: "0.0.0.0".into(),
    }
}

/// The address of a `c=IN IP4 <addr>` value.
fn c_address(value: &str) -> Option<String> {
    value.split_whitespace().nth(2).map(str::to_string)
}

/// One stream of `doc` in the negotiator's model.
fn media_of(doc: &sip_message::SdpDoc, m: &sip_message::MediaLine) -> SdpMedia {
    let or = |value: &str, default: &str| {
        if value.is_empty() {
            default.to_string()
        } else {
            value.to_string()
        }
    };
    let rtpmap = sip_message::extract_rtpmaps(m)
        .into_iter()
        .filter_map(|(pt, encoding)| {
            // <name>/<rate>[/<channels>]
            let mut parts = encoding.split('/');
            let name = parts.next().unwrap_or("");
            let rate = parts.next().and_then(|r| r.parse::<u32>().ok()).unwrap_or(8000);
            Some(SdpRtpMap {
                payload_type: pt.parse().ok()?,
                encoding_name: name.to_string(),
                clock_rate: rate,
            })
        })
        .collect();
    SdpMedia {
        kind: or(&m.r#type, "audio"),
        port: m.port.and_then(|p| u16::try_from(p).ok()).unwrap_or(0),
        protocol: or(&m.transport, "RTP/AVP"),
        formats: m.formats.iter().filter_map(|f| f.parse::<u8>().ok()).collect(),
        rtpmap,
        direction: match sip_message::extract_direction(doc, m) {
            sip_message::SdpDirection::SendRecv => MediaDirection::SendRecv,
            sip_message::SdpDirection::SendOnly => MediaDirection::SendOnly,
            sip_message::SdpDirection::RecvOnly => MediaDirection::RecvOnly,
            sip_message::SdpDirection::Inactive => MediaDirection::Inactive,
        },
        connection_addr: m.c_line.as_deref().and_then(c_address),
    }
}

/// Serialise an [`Sdp`] to wire text (CRLF line endings).
pub fn build_sdp(sdp: &Sdp) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push("v=0".into());
    lines.push(format!(
        "o={} {} {} IN IP4 {}",
        sdp.origin.username, sdp.origin.session_id, sdp.origin.version, sdp.origin.address
    ));
    lines.push("s=-".into());
    if let Some(c) = &sdp.connection_addr {
        lines.push(format!("c=IN IP4 {c}"));
    }
    lines.push("t=0 0".into());
    for m in &sdp.media {
        let formats: Vec<String> = m.formats.iter().map(|f| f.to_string()).collect();
        lines.push(format!("m={} {} {} {}", m.kind, m.port, m.protocol, formats.join(" ")));
        if let Some(c) = &m.connection_addr {
            lines.push(format!("c=IN IP4 {c}"));
        }
        for r in &m.rtpmap {
            lines.push(format!("a=rtpmap:{} {}/{}", r.payload_type, r.encoding_name, r.clock_rate));
        }
        lines.push(format!("a={}", m.direction.as_str()));
    }
    lines.join("\r\n") + "\r\n"
}

/// Serialise to bytes (the wire body).
pub fn encode_sdp(sdp: &Sdp) -> Vec<u8> {
    build_sdp(sdp).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_build_parse() {
        let sdp = Sdp {
            origin: SdpOrigin {
                username: "media".into(),
                session_id: "42".into(),
                version: "2".into(),
                address: "10.10.0.1".into(),
            },
            connection_addr: Some("10.10.0.1".into()),
            media: vec![SdpMedia {
                kind: "audio".into(),
                port: 40000,
                protocol: "RTP/AVP".into(),
                formats: vec![8, 0],
                rtpmap: vec![
                    SdpRtpMap { payload_type: 8, encoding_name: "PCMA".into(), clock_rate: 8000 },
                    SdpRtpMap { payload_type: 0, encoding_name: "PCMU".into(), clock_rate: 8000 },
                ],
                direction: MediaDirection::SendRecv,
                connection_addr: None,
            }],
        };
        let reparsed = parse_sdp(&build_sdp(&sdp));
        assert_eq!(reparsed, sdp);
    }

    /// RFC 4566 §5: the session-level direction is every stream's default;
    /// RFC 8866 §5.14: a port field may state its number of ports.
    #[test]
    fn a_stream_reads_the_session_direction_and_a_counted_port() {
        let sdp = parse_sdp(
            "v=0\r\no=- 0 0 IN IP4 192.0.2.1\r\ns=-\r\nc=IN IP4 192.0.2.1\r\nt=0 0\r\n\
             a=sendonly\r\nm=audio 49170/2 RTP/AVP 8\r\nm=audio 49174 RTP/AVP 0\r\na=inactive\r\n",
        );
        assert_eq!(sdp.media[0].port, 49170);
        assert_eq!(sdp.media[0].direction, MediaDirection::SendOnly);
        assert_eq!(sdp.media[1].direction, MediaDirection::Inactive, "its own wins");
    }

    #[test]
    fn media_level_connection_addr_wins() {
        let mut sdp = parse_sdp("v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\nc=IN IP4 2.2.2.2\r\nm=audio 5000 RTP/AVP 8\r\nc=IN IP4 3.3.3.3\r\na=sendrecv\r\n");
        let m = sdp.media.remove(0);
        assert_eq!(media_connection_addr(&parse_sdp("v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\nc=IN IP4 2.2.2.2\r\nm=audio 5000 RTP/AVP 8\r\nc=IN IP4 3.3.3.3\r\na=sendrecv\r\n"), &m), "3.3.3.3");
    }
}
