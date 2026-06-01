//! Structured SDP model, parser and builder for the negotiation engine.
//!
//! Port of `../sipjsserver/src/media/sdp/{types,parse}.ts`. Handles the subset a
//! plain RTP/AVP audio UA emits and reads: `v= o= s= c= t= m=audio` plus
//! `a=rtpmap` and direction attributes. Round-trippable. Deliberately
//! independent of the b2bua-scoped string SDP in `sip-message` — this is the
//! conformance witness the negotiator validates against.

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

/// Parse SDP text. Infallible like the TS parser — unknown lines are ignored and
/// missing fields take RFC defaults; all conformance checks live in the
/// negotiator.
pub fn parse_sdp(input: &str) -> Sdp {
    let mut origin = SdpOrigin {
        username: "-".into(),
        session_id: "0".into(),
        version: "0".into(),
        address: "0.0.0.0".into(),
    };
    let mut session_conn: Option<String> = None;
    let mut media: Vec<SdpMedia> = Vec::new();
    let mut cur: Option<SdpMedia> = None;

    for line in input.split(['\r', '\n']).filter(|l| !l.is_empty()) {
        let bytes = line.as_bytes();
        if bytes.len() < 2 || bytes[1] != b'=' {
            continue;
        }
        let kind = bytes[0];
        let value = &line[2..];
        match kind {
            b'o' => {
                // o=<user> <sess-id> <sess-version> IN IP4 <addr>
                let p: Vec<&str> = value.split_whitespace().collect();
                origin = SdpOrigin {
                    username: p.first().copied().unwrap_or("-").to_string(),
                    session_id: p.get(1).copied().unwrap_or("0").to_string(),
                    version: p.get(2).copied().unwrap_or("0").to_string(),
                    address: p.get(5).copied().unwrap_or("0.0.0.0").to_string(),
                };
            }
            b'c' => {
                // c=IN IP4 <addr>
                if let Some(addr) = value.split_whitespace().nth(2) {
                    match cur.as_mut() {
                        Some(m) => m.connection_addr = Some(addr.to_string()),
                        None => session_conn = Some(addr.to_string()),
                    }
                }
            }
            b'm' => {
                if let Some(done) = cur.take() {
                    media.push(done);
                }
                let p: Vec<&str> = value.split_whitespace().collect();
                cur = Some(SdpMedia {
                    kind: p.first().copied().unwrap_or("audio").to_string(),
                    port: p.get(1).and_then(|s| s.parse().ok()).unwrap_or(0),
                    protocol: p.get(2).copied().unwrap_or("RTP/AVP").to_string(),
                    formats: p
                        .iter()
                        .skip(3)
                        .filter_map(|f| f.parse::<u8>().ok())
                        .collect(),
                    rtpmap: Vec::new(),
                    direction: MediaDirection::SendRecv, // RFC 3264 default
                    connection_addr: None,
                });
            }
            b'a' => {
                if let Some(rest) = value.strip_prefix("rtpmap:") {
                    // a=rtpmap:<pt> <name>/<rate>[/<channels>]
                    if let Some(sp) = rest.find(' ') {
                        let pt = rest[..sp].parse::<u8>().ok();
                        let tail = &rest[sp + 1..];
                        let mut parts = tail.split('/');
                        let name = parts.next().unwrap_or("");
                        let rate = parts.next().and_then(|r| r.parse::<u32>().ok()).unwrap_or(8000);
                        if let (Some(pt), Some(m)) = (pt, cur.as_mut()) {
                            m.rtpmap.push(SdpRtpMap {
                                payload_type: pt,
                                encoding_name: name.to_string(),
                                clock_rate: rate,
                            });
                        }
                    }
                } else if let Some(dir) = MediaDirection::from_str(value) {
                    if let Some(m) = cur.as_mut() {
                        m.direction = dir;
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(done) = cur.take() {
        media.push(done);
    }

    Sdp {
        origin,
        connection_addr: session_conn,
        media,
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
        lines.push(format!(
            "m={} {} {} {}",
            m.kind,
            m.port,
            m.protocol,
            formats.join(" ")
        ));
        if let Some(c) = &m.connection_addr {
            lines.push(format!("c=IN IP4 {c}"));
        }
        for r in &m.rtpmap {
            lines.push(format!(
                "a=rtpmap:{} {}/{}",
                r.payload_type, r.encoding_name, r.clock_rate
            ));
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

    #[test]
    fn media_level_connection_addr_wins() {
        let mut sdp = parse_sdp("v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\nc=IN IP4 2.2.2.2\r\nm=audio 5000 RTP/AVP 8\r\nc=IN IP4 3.3.3.3\r\na=sendrecv\r\n");
        let m = sdp.media.remove(0);
        assert_eq!(media_connection_addr(&parse_sdp("v=0\r\no=- 0 0 IN IP4 1.1.1.1\r\nc=IN IP4 2.2.2.2\r\nm=audio 5000 RTP/AVP 8\r\nc=IN IP4 3.3.3.3\r\na=sendrecv\r\n"), &m), "3.3.3.3");
    }
}
