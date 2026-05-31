//! SDP answer construction for the fake-PRACK UPDATE handler — port of
//! `src/sip/SdpAnswerFromOffer.ts`.
//!
//! Given bob's UPDATE SDP offer and alice's INVITE SDP, build a syntactically
//! valid RFC 3264 answer: m-line count/order match bob's offer; per-m-line codec
//! list = bob's offer ∩ alice's INVITE (matched by codec name + clock rate);
//! port and `c=` come from alice. Empty intersection on any m-line →
//! `NoCommonCodec`, and the caller replies 488.

const CRLF: &str = "\r\n";

/// RFC 3551 static payload types (subset our test traffic emits).
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

#[derive(Debug)]
struct MediaSection {
    media: String,
    port: i64,
    proto: String,
    payload_types: Vec<i64>,
    connection: Option<String>,
    rtpmaps: Vec<(i64, String)>,
    fmtps: Vec<(i64, String)>,
    direction: Option<String>,
}

struct ParsedSdp {
    session_connection: Option<String>,
    media_sections: Vec<MediaSection>,
}

/// Result of [`build_answer_from_offer`].
#[derive(Debug, PartialEq, Eq)]
pub enum SdpBuildResult {
    Ok(Vec<u8>),
    NoCommonCodec { m_line_index: usize },
    NoAliceSdp,
}

fn split_lines(text: &str) -> Vec<&str> {
    text.split(['\r', '\n']).filter(|l| !l.is_empty()).collect()
}

fn parse_sdp(text: &str) -> ParsedSdp {
    let lines = split_lines(text);
    let mut session_connection: Option<String> = None;
    let mut media_sections: Vec<MediaSection> = Vec::new();

    let mut i = 0;
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
        let parts: Vec<&str> = m_line[2..].trim().split_whitespace().collect();
        let media = parts.first().copied().unwrap_or("").to_string();
        let port = parts.get(1).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        let proto = parts.get(2).copied().unwrap_or("RTP/AVP").to_string();
        let payload_types: Vec<i64> = parts
            .iter()
            .skip(3)
            .filter_map(|f| f.parse::<i64>().ok())
            .collect();

        let mut connection: Option<String> = None;
        let mut rtpmaps: Vec<(i64, String)> = Vec::new();
        let mut fmtps: Vec<(i64, String)> = Vec::new();
        let mut direction: Option<String> = None;

        while i < lines.len() && !lines[i].starts_with("m=") {
            let line = lines[i];
            if line.starts_with("c=") && connection.is_none() {
                connection = Some(line.to_string());
            } else if let Some(rest) = line.strip_prefix("a=rtpmap:") {
                let rest = rest.trim();
                if let Some(space) = rest.find(' ') {
                    if let Ok(pt) = rest[..space].parse::<i64>() {
                        let codec = rest[space + 1..].trim().to_string();
                        if !codec.is_empty() {
                            rtpmaps.push((pt, codec));
                        }
                    }
                }
            } else if let Some(rest) = line.strip_prefix("a=fmtp:") {
                let rest = rest.trim();
                if let Some(space) = rest.find(' ') {
                    if let Ok(pt) = rest[..space].parse::<i64>() {
                        fmtps.push((pt, line.to_string()));
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

    ParsedSdp {
        session_connection,
        media_sections,
    }
}

fn codec_key(pt: i64, rtpmaps: &[(i64, String)]) -> Option<String> {
    if let Some((_, c)) = rtpmaps.iter().find(|(p, _)| *p == pt) {
        return Some(c.to_lowercase());
    }
    static_pt(pt).map(|s| s.to_lowercase())
}

/// Codec intersection (by name+rate) keeping bob's payload types, or `None` if
/// empty.
fn intersect_codecs(bob: &MediaSection, alice: &MediaSection) -> Option<Vec<i64>> {
    let alice_keys: std::collections::HashSet<String> = alice
        .payload_types
        .iter()
        .filter_map(|pt| codec_key(*pt, &alice.rtpmaps))
        .collect();
    let matched: Vec<i64> = bob
        .payload_types
        .iter()
        .filter(|pt| {
            codec_key(**pt, &bob.rtpmaps)
                .map(|k| alice_keys.contains(&k))
                .unwrap_or(false)
        })
        .copied()
        .collect();
    if matched.is_empty() {
        None
    } else {
        Some(matched)
    }
}

fn build_answer_section(
    bob: &MediaSection,
    alice: Option<&MediaSection>,
    alice_session_connection: Option<&str>,
    matched_pts: Option<&[i64]>,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    match (matched_pts, alice) {
        (Some(pts), Some(alice)) => {
            let pts_str = pts
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            lines.push(format!("m={} {} {} {}", bob.media, alice.port, bob.proto, pts_str));
            if let Some(c) = alice
                .connection
                .as_deref()
                .or(alice_session_connection)
            {
                lines.push(c.to_string());
            }
            for pt in pts {
                if let Some((_, rtpmap)) = bob.rtpmaps.iter().find(|(p, _)| p == pt) {
                    lines.push(format!("a=rtpmap:{pt} {rtpmap}"));
                }
                if let Some((_, fmtp)) = bob.fmtps.iter().find(|(p, _)| p == pt) {
                    lines.push(fmtp.clone());
                }
            }
            lines.push(format!("a={}", bob.direction.as_deref().unwrap_or("sendrecv")));
        }
        _ => {
            // Disabled placeholder (RFC 3264 §6): port 0, a=inactive.
            match bob.payload_types.first() {
                None => lines.push(format!("m={} 0 {} 0", bob.media, bob.proto)),
                Some(pt) => {
                    lines.push(format!("m={} 0 {} {}", bob.media, bob.proto, pt));
                    if let Some((_, rtpmap)) = bob.rtpmaps.iter().find(|(p, _)| p == pt) {
                        lines.push(format!("a=rtpmap:{pt} {rtpmap}"));
                    }
                }
            }
            lines.push("a=inactive".to_string());
        }
    }
    lines.join(CRLF)
}

fn sdp_origin_address(local_ip: &str) -> String {
    if local_ip == "0.0.0.0" || local_ip == "::" {
        "127.0.0.1".to_string()
    } else {
        local_ip.to_string()
    }
}

fn sdp_session_id(now_ms: i64) -> i64 {
    let sec = now_ms / 1000;
    if sec > 0 {
        sec
    } else {
        1
    }
}

/// Build an answer to `bob_offer` whose addresses/ports come from `alice_offer`.
/// Empty `alice_offer` → [`SdpBuildResult::NoAliceSdp`]; an m-line with no codec
/// overlap → [`SdpBuildResult::NoCommonCodec`].
pub fn build_answer_from_offer(
    bob_offer: &[u8],
    alice_offer: &[u8],
    local_ip: &str,
    now_ms: i64,
) -> SdpBuildResult {
    if alice_offer.is_empty() {
        return SdpBuildResult::NoAliceSdp;
    }
    let alice_text = String::from_utf8_lossy(alice_offer);
    if alice_text.is_empty() {
        return SdpBuildResult::NoAliceSdp;
    }
    let bob_text = String::from_utf8_lossy(bob_offer);
    let bob = parse_sdp(&bob_text);
    let alice = parse_sdp(&alice_text);

    if bob.media_sections.is_empty() || alice.media_sections.is_empty() {
        return SdpBuildResult::NoAliceSdp;
    }

    let mut sections: Vec<String> = Vec::new();
    for (idx, bob_section) in bob.media_sections.iter().enumerate() {
        let alice_section = alice.media_sections.get(idx);
        match alice_section {
            Some(alice_section) => match intersect_codecs(bob_section, alice_section) {
                None => return SdpBuildResult::NoCommonCodec { m_line_index: idx },
                Some(pts) => sections.push(build_answer_section(
                    bob_section,
                    Some(alice_section),
                    alice.session_connection.as_deref(),
                    Some(&pts),
                )),
            },
            None => sections.push(build_answer_section(
                bob_section,
                None,
                alice.session_connection.as_deref(),
                None,
            )),
        }
    }

    let origin_ip = sdp_origin_address(local_ip);
    let sess_id = sdp_session_id(now_ms);
    let session_lines = vec![
        "v=0".to_string(),
        format!("o=b2bua {sess_id} {sess_id} IN IP4 {origin_ip}"),
        "s=-".to_string(),
        "t=0 0".to_string(),
    ];
    let body = session_lines.join(CRLF) + CRLF + &sections.join(CRLF) + CRLF;
    SdpBuildResult::Ok(body.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8 18 101\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:18 G729/8000\r\na=rtpmap:101 telephone-event/8000\r\n";
    const BOB_OK: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
    const BOB_OPUS: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 96\r\na=rtpmap:96 opus/48000/2\r\na=sendrecv\r\n";

    #[test]
    fn ok_intersection_uses_alice_port() {
        let r = build_answer_from_offer(BOB_OK.as_bytes(), ALICE.as_bytes(), "127.0.0.1", 1000);
        match r {
            SdpBuildResult::Ok(body) => {
                let s = String::from_utf8(body).unwrap();
                assert!(s.contains("m=audio 10000 RTP/AVP 8"), "{s}");
                assert!(s.contains("a=rtpmap:8 PCMA/8000"), "{s}");
            }
            other => panic!("expected ok, got {other:?}"),
        }
    }

    #[test]
    fn no_overlap_is_no_common_codec() {
        let r = build_answer_from_offer(BOB_OPUS.as_bytes(), ALICE.as_bytes(), "127.0.0.1", 1000);
        assert_eq!(r, SdpBuildResult::NoCommonCodec { m_line_index: 0 });
    }

    #[test]
    fn empty_alice_is_no_alice_sdp() {
        let r = build_answer_from_offer(BOB_OK.as_bytes(), b"", "127.0.0.1", 1000);
        assert_eq!(r, SdpBuildResult::NoAliceSdp);
    }
}
