//! Answers a back-to-back UA composes on a party's behalf (RFC 3264 §6).
//!
//! [`answer_from_own`] answers an offer out of a description the answering
//! party already stated — its own earlier offer — so the answer speaks with that
//! party's addresses, ports and attributes: one m-line per offered stream, each
//! live stream reduced to ONE format both ends listed (the one ranked first by
//! the side the caller names) plus a common `telephone-event` on audio, the
//! direction both ends allow. [`reject_offer`] answers an offer rejecting every
//! stream.
//!
//! Every value is read through [`crate::sdp_doc`]; the own description's lines
//! are carried byte for byte except the ones an answer must change. Pure and
//! deterministic: no clocks, no randomness, no I/O.

use crate::sdp_doc::{
    canonical_rtpmap, direction_of, extract_direction, extract_rtpmaps, parse_sdp_body, MediaLine,
    SdpDirection, Sections,
};

/// Whose order of preference picks the one format an answered stream keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatPreference {
    /// The first of the offer's formats the own description also lists.
    Offerer,
    /// The first of the own description's formats the offer also lists.
    Answerer,
}

/// The answer to `offer` built out of `own`, a description the answering party
/// stated earlier, or `None` where either is not a session description.
///
/// Session-level lines are `own`'s, its `o=` value replaced by `origin` where
/// one is given. The answer has one m-line per offered stream, in order,
/// matched to `own`'s stream of the same rank:
///
/// - an offered stream with port 0 is answered with its own m-line;
/// - one `own` does not describe at that rank, or describes with another media
///   type, or with no format in common, is answered rejected: its m-line with
///   port 0;
/// - one `own` describes with port 0 is answered with `own`'s m-line;
/// - otherwise `own`'s media section answers it with `own`'s port and
///   transport and the formats [`FormatPreference`] keeps, `own`'s
///   `rtpmap`/`fmtp` lines for those formats only, its other lines as written,
///   and the direction [`answer_direction`] gives, written in place of `own`'s
///   media-level one or added where it differs from what the stream inherits.
///
/// A format is common when both streams list it and, where both bind it to an
/// encoding (an `rtpmap`, or the RFC 3551 static assignment), the encodings are
/// equal. A rejected m-line carries no attribute, and `own`'s first media-level
/// `c=` line where `own` states no session-level one (RFC 4566 §5.7).
pub fn answer_from_own(
    offer: &[u8],
    own: &[u8],
    preference: FormatPreference,
    origin: Option<&str>,
) -> Option<Vec<u8>> {
    let offer_doc = parse_sdp_body(offer)?;
    let own_doc = parse_sdp_body(own)?;
    let offer_text = String::from_utf8_lossy(offer);
    let own_text = String::from_utf8_lossy(own);
    let offered = Sections::of(&offer_text);
    let owned = Sections::of(&own_text);
    let eol = if own_text.contains("\r\n") { "\r\n" } else { "\n" };

    let mut out = String::new();
    for line in lines(owned.session) {
        match (line.starts_with("o="), origin) {
            (true, Some(o)) => push_line(&mut out, &format!("o={o}"), eol),
            _ => push_line(&mut out, line, eol),
        }
    }
    let own_session_dir = session_direction(owned.session);
    let offer_session_dir = session_direction(offered.session);
    let stub_c = own_doc
        .c_line
        .is_none()
        .then(|| own_doc.media.iter().find_map(|m| m.c_line.clone()))
        .flatten();
    let reject = |out: &mut String, m_value: String| {
        push_line(out, &format!("m={m_value}"), eol);
        if let Some(c) = &stub_c {
            push_line(out, &format!("c={c}"), eol);
        }
    };

    for (i, stream) in offer_doc.media.iter().enumerate() {
        let offered_value = offered.media[i].value();
        if stream.port == Some(0) {
            reject(&mut out, offered_value.to_string());
            continue;
        }
        let Some(mine) = own_doc.media.get(i).filter(|m| m.r#type == stream.r#type) else {
            reject(&mut out, with_port_zero(offered_value));
            continue;
        };
        if mine.port == Some(0) {
            reject(&mut out, owned.media[i].value().to_string());
            continue;
        }
        let Some(kept) = kept_formats(stream, mine, preference) else {
            reject(&mut out, with_port_zero(offered_value));
            continue;
        };
        let inherited = own_session_dir.unwrap_or(SdpDirection::SendRecv);
        let own_dir = media_direction(mine).unwrap_or(inherited);
        let offered_dir =
            media_direction(stream).or(offer_session_dir).unwrap_or(SdpDirection::SendRecv);
        let dir = answer_direction(own_dir, offered_dir);
        answered_section(&mut out, owned.media[i].text, mine, &kept, dir, inherited, eol);
    }
    Some(out.into_bytes())
}

/// The direction an answerer states for a stream it can use `own`-wise, offered
/// `offered` (RFC 3264 §6.1): it sends where it can send and the offerer
/// receives, it receives where it can receive and the offerer sends.
pub fn answer_direction(own: SdpDirection, offered: SdpDirection) -> SdpDirection {
    let sends = |d| matches!(d, SdpDirection::SendRecv | SdpDirection::SendOnly);
    let receives = |d| matches!(d, SdpDirection::SendRecv | SdpDirection::RecvOnly);
    match (sends(own) && receives(offered), receives(own) && sends(offered)) {
        (true, true) => SdpDirection::SendRecv,
        (true, false) => SdpDirection::SendOnly,
        (false, true) => SdpDirection::RecvOnly,
        (false, false) => SdpDirection::Inactive,
    }
}

/// The answer to `offer` rejecting each of its streams (RFC 3264 §6): every
/// offered m-line with port 0, its formats and `a=inactive`, under `origin`
/// (the `o=` value) and a session-level `c=` naming `address`. `None` where
/// `offer` is not a session description.
pub fn reject_offer(offer: &[u8], origin: &str, address: &str) -> Option<Vec<u8>> {
    let doc = parse_sdp_body(offer)?;
    let text = String::from_utf8_lossy(offer);
    let sections = Sections::of(&text);
    let addrtype = if address.contains(':') { "IP6" } else { "IP4" };
    let mut out = String::new();
    for line in
        ["v=0", &format!("o={origin}"), "s=-", &format!("c=IN {addrtype} {address}"), "t=0 0"]
    {
        push_line(&mut out, line, "\r\n");
    }
    for (stream, section) in doc.media.iter().zip(&sections.media) {
        let value = match stream.port {
            Some(0) => section.value().to_string(),
            _ => with_port_zero(section.value()),
        };
        push_line(&mut out, &format!("m={value}"), "\r\n");
        push_line(&mut out, "a=inactive", "\r\n");
    }
    Some(out.into_bytes())
}

/// The formats an answered stream keeps: the first common format in the
/// preferred order that is not `telephone-event`, then, on audio, the first
/// common `telephone-event` in that order. `None` where no such first format
/// exists: `telephone-event` alone carries no media.
fn kept_formats(
    offered: &MediaLine,
    own: &MediaLine,
    preference: FormatPreference,
) -> Option<Vec<String>> {
    let offered_maps = extract_rtpmaps(offered);
    let own_maps = extract_rtpmaps(own);
    let encoding = |f: &str, m: &MediaLine, maps: &[(String, String)]| {
        maps.iter()
            .find(|(pt, _)| pt == f)
            .map(|(_, enc)| canonical_rtpmap(&m.r#type, enc))
            .or_else(|| static_encoding(f).map(|enc| canonical_rtpmap(&m.r#type, enc)))
            .map(|enc| enc.to_ascii_lowercase())
    };
    let common = |f: &str| {
        offered.formats.iter().any(|o| o == f)
            && own.formats.iter().any(|o| o == f)
            && match (encoding(f, offered, &offered_maps), encoding(f, own, &own_maps)) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    };
    let events = |f: &str| {
        [encoding(f, offered, &offered_maps), encoding(f, own, &own_maps)]
            .into_iter()
            .flatten()
            .any(|enc| enc.split('/').next() == Some("telephone-event"))
    };
    let ranked = match preference {
        FormatPreference::Offerer => &offered.formats,
        FormatPreference::Answerer => &own.formats,
    };
    let codec = ranked.iter().find(|f| common(f) && !events(f))?;
    let mut kept = vec![codec.clone()];
    if offered.r#type.eq_ignore_ascii_case("audio") {
        if let Some(te) = ranked.iter().find(|f| common(f) && events(f)) {
            kept.push(te.clone());
        }
    }
    Some(kept)
}

/// `own`'s media section `text` answering with `kept` formats and `dir`: the
/// m-line's format list replaced, `rtpmap`/`fmtp` lines kept for `kept` only,
/// the first direction attribute written as `dir` and any other dropped, `dir`
/// added at the end where the section states none and `inherited` differs.
fn answered_section(
    out: &mut String,
    text: &str,
    own: &MediaLine,
    kept: &[String],
    dir: SdpDirection,
    inherited: SdpDirection,
    eol: &str,
) {
    let mut section = lines(text);
    let m_value = section.next().and_then(|l| l.get(2..)).unwrap_or_default();
    let port_field = m_value.split_whitespace().nth(1).unwrap_or("0");
    push_line(
        out,
        &format!("m={} {port_field} {} {}", own.r#type, own.transport, kept.join(" ")),
        eol,
    );
    let mut dir_written = false;
    for line in section {
        let attr = line.strip_prefix("a=");
        let bound_pt = attr
            .and_then(|a| a.strip_prefix("rtpmap:").or_else(|| a.strip_prefix("fmtp:")))
            .map(|rest| rest.split_whitespace().next().unwrap_or_default());
        match (bound_pt, attr.and_then(direction_of)) {
            (Some(pt), _) if !kept.iter().any(|k| k == pt) => {}
            (_, Some(_)) if dir_written => {}
            (_, Some(_)) => {
                push_line(out, &format!("a={}", dir.token()), eol);
                dir_written = true;
            }
            _ => push_line(out, line, eol),
        }
    }
    if !dir_written && dir != inherited {
        push_line(out, &format!("a={}", dir.token()), eol);
    }
}

/// The direction a media block states for itself, if any.
fn media_direction(media: &MediaLine) -> Option<SdpDirection> {
    media.attributes.iter().any(|a| direction_of(a).is_some()).then(|| extract_direction(media))
}

/// The direction the session-level lines state, if any.
fn session_direction(session: &str) -> Option<SdpDirection> {
    lines(session).find_map(|l| l.strip_prefix("a=").and_then(direction_of))
}

/// An `m=` value with its port set to 0, every other token as written.
fn with_port_zero(m_value: &str) -> String {
    let mut tokens = m_value.split_whitespace();
    let kind = tokens.next().unwrap_or_default();
    let _port = tokens.next();
    let rest: Vec<&str> = tokens.collect();
    if rest.is_empty() {
        format!("{kind} 0")
    } else {
        format!("{kind} 0 {}", rest.join(" "))
    }
}

/// The lines of `text`, line endings removed.
fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).filter(|l| !l.is_empty())
}

fn push_line(out: &mut String, line: &str, eol: &str) {
    out.push_str(line);
    out.push_str(eol);
}

/// The encoding RFC 3551 §6 (tables 4 and 5) assigns a static payload type.
fn static_encoding(format: &str) -> Option<&'static str> {
    Some(match format.parse::<u8>().ok()? {
        0 => "PCMU/8000",
        3 => "GSM/8000",
        4 => "G723/8000",
        5 => "DVI4/8000",
        6 => "DVI4/16000",
        7 => "LPC/8000",
        8 => "PCMA/8000",
        9 => "G722/8000",
        10 => "L16/44100/2",
        11 => "L16/44100",
        12 => "QCELP/8000",
        13 => "CN/8000",
        14 => "MPA/90000",
        15 => "G728/8000",
        16 => "DVI4/11025",
        17 => "DVI4/22050",
        18 => "G729/8000",
        25 => "CelB/90000",
        26 => "JPEG/90000",
        28 => "nv/90000",
        31 => "H261/90000",
        32 => "MPV/90000",
        33 => "MP2T/90000",
        34 => "H263/90000",
        _ => return None,
    })
}
