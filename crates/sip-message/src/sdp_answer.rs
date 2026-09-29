//! Answers a back-to-back UA composes on a party's behalf (RFC 3264 §6).
//!
//! [`answer_from_own`] answers an offer out of a description the answering
//! party already stated — its own earlier offer — so the answer speaks with that
//! party's addresses, ports and attributes: one m-line per offered stream, each
//! live stream reduced to ONE format both ends listed (the one ranked first by
//! the side the caller names) plus a common `telephone-event` on audio, the
//! direction both ends allow. [`reject_offer`] answers an offer rejecting every
//! stream; [`has_live_stream`] tells an answer that accepts something from one
//! that rejects every stream.
//!
//! Every value is read through [`crate::sdp_doc`]; the own description's lines
//! are carried byte for byte except the ones an answer must change. Pure and
//! deterministic: no clocks, no randomness, no I/O.

use crate::sdp::{sdp_origin_address, sdp_session_id, BuildHeldSdpOptions};
use crate::sdp_doc::{
    canonical_rtpmap, direction_of, extract_cryptos, extract_direction, extract_fmtps,
    extract_rtpmaps, parse_sdp_body, MediaLine, SdpDirection, Sections,
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
///   type or transport profile, or with no format in common, or offers SDES
///   keys (RFC 4568) for no suite `own` holds, or either side sets up over
///   DTLS (out of scope: the handshake role and certificate are the party's
///   own), is answered rejected: its m-line with port 0;
/// - one `own` describes with port 0 is answered with `own`'s m-line;
/// - otherwise `own`'s media section answers it with `own`'s port and
///   transport and the formats [`FormatPreference`] keeps, under the offer's
///   numbers (RFC 3264 §6.1); `own`'s per-format attributes (`rtpmap`, `fmtp`,
///   `rtcp-fb`) for those formats only, renumbered, an `rtpmap` spelled as the
///   offer spells it, and the offer's `rtpmap` for a dynamic number `own`
///   binds none for; ONE `crypto` line, `own`'s key for the first suite both
///   hold in the [`FormatPreference`] order, under the offer's tag, where the
///   offer carries SDES; the offer's `mid` in place of `own`'s (RFC 5888
///   §9.2); its other lines as written (ICE attributes included: the answer
///   speaks for `own`'s transport); and the direction [`answer_direction`]
///   gives, written in place of `own`'s media-level one or added where it
///   differs from what the stream inherits.
///
/// `own`'s session-level `group` lines are left out: they group `own`'s
/// streams, not the offer's (RFC 5888 §9.2).
///
/// A static format (below 96) is common by number; a dynamic one by encoding
/// (name, clock rate, channels, RFC 4566 §6), whatever number each side gives
/// it; any other format token by equality. AMR and AMR-WB formats are common
/// only in the same `octet-align` mode (RFC 4867 §8.3.1); other `fmtp`
/// parameters are not compared. A rejected m-line carries no attribute, and
/// `own`'s first media-level `c=` line where `own` states no session-level one
/// (RFC 4566 §5.7).
pub fn answer_from_own(
    offer: &[u8],
    own: &[u8],
    preference: FormatPreference,
    origin: Option<&str>,
) -> Option<Vec<u8>> {
    answer_from_own_agreeing(offer, own, preference, origin, None)
}

/// [`answer_from_own`], each stream's SDES suite the one `agreed` (the other
/// answer of the same exchange, one m-line per stream of the same rank) keeps
/// there, so both ends of a back-to-back exchange run one SRTP context; a
/// stream whose `agreed` counterpart keeps no suite chooses as
/// [`answer_from_own`] does.
pub fn answer_from_own_agreeing(
    offer: &[u8],
    own: &[u8],
    preference: FormatPreference,
    origin: Option<&str>,
    agreed: Option<&[u8]>,
) -> Option<Vec<u8>> {
    let agreed_suites: Vec<Option<String>> = agreed
        .and_then(parse_sdp_body)
        .map(|d| {
            d.media
                .iter()
                .map(|m| extract_cryptos(m).into_iter().next().map(|(_, s, _)| s))
                .collect()
        })
        .unwrap_or_default();
    let offer_doc = parse_sdp_body(offer)?;
    let own_doc = parse_sdp_body(own)?;
    let offer_text = String::from_utf8_lossy(offer);
    let own_text = String::from_utf8_lossy(own);
    let offered = Sections::of(&offer_text);
    let owned = Sections::of(&own_text);
    let eol = if own_text.contains("\r\n") { "\r\n" } else { "\n" };

    let mut out = String::new();
    // An own `group` answers nothing the offer grouped (RFC 5888 §9.2).
    for line in lines(owned.session).filter(|l| !l.starts_with("a=group:")) {
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
        let answerable = mine.transport == stream.transport
            && !over_dtls(stream, offered.session)
            && !over_dtls(mine, owned.session);
        let kept = answerable.then(|| kept_formats(stream, mine, preference)).flatten();
        let required = agreed_suites.get(i).cloned().flatten();
        let crypto = sdes_answer(stream, mine, preference, required.as_deref());
        let (Some(kept), Some(crypto)) = (kept, crypto) else {
            reject(&mut out, with_port_zero(offered_value));
            continue;
        };
        let inherited = own_session_dir.unwrap_or(SdpDirection::SendRecv);
        let own_dir = media_direction(mine).unwrap_or(inherited);
        let offered_dir =
            media_direction(stream).or(offer_session_dir).unwrap_or(SdpDirection::SendRecv);
        let dir = answer_direction(own_dir, offered_dir);
        let answer = Answered { stream, own: mine, kept: &kept, crypto, dir, inherited };
        answered_section(&mut out, owned.media[i].text, &answer, eol);
    }
    Some(out.into_bytes())
}

/// Whether `sdp` is a session description with a stream not rejected (a
/// non-zero port, RFC 3264 §6).
pub fn has_live_stream(sdp: &[u8]) -> bool {
    parse_sdp_body(sdp).is_some_and(|d| d.media.iter().any(|m| m.port.is_some_and(|p| p != 0)))
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
/// offered m-line with port 0, its formats and `a=inactive`, under an origin
/// and a session-level `c=` of the answerer's own (`options`' address, a
/// session id from its clock). `None` where `offer` is not a session
/// description.
pub fn reject_offer(offer: &[u8], options: &BuildHeldSdpOptions) -> Option<Vec<u8>> {
    let doc = parse_sdp_body(offer)?;
    let text = String::from_utf8_lossy(offer);
    let sections = Sections::of(&text);
    let address = sdp_origin_address(&options.local_ip);
    let addrtype = if address.contains(':') { "IP6" } else { "IP4" };
    let sess = sdp_session_id(options.now_ms);
    let mut out = String::new();
    for line in [
        "v=0",
        &format!("o=b2bua {sess} {sess} IN {addrtype} {address}"),
        "s=-",
        &format!("c=IN {addrtype} {address}"),
        "t=0 0",
    ] {
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

/// One kept format: the number the offer gives it and the one `own` gives it.
struct Kept {
    offered: String,
    own: String,
}

/// The encoding a format of `media` names, lower-cased and canonical (RFC 4566
/// §6): its `rtpmap`, else the RFC 3551 static assignment.
fn encoding(format: &str, media: &MediaLine) -> Option<String> {
    extract_rtpmaps(media)
        .into_iter()
        .find(|(pt, _)| pt == format)
        .map(|(_, enc)| canonical_rtpmap(&media.r#type, &enc))
        .or_else(|| static_encoding(format).map(|enc| canonical_rtpmap(&media.r#type, enc)))
        .map(|enc| enc.to_ascii_lowercase())
}

/// Whether `format` is a dynamic RTP payload type (RFC 3551 §3: 96–127).
fn is_dynamic(format: &str) -> bool {
    format.parse::<u8>().is_ok_and(|pt| pt >= 96)
}

/// The `octet-align` mode an AMR or AMR-WB format states (RFC 4867 §8.1: absent
/// is 0), or `None` for any other encoding.
fn amr_alignment(format: &str, media: &MediaLine, enc: &str) -> Option<String> {
    let name = enc.split('/').next()?;
    if name != "amr" && name != "amr-wb" {
        return None;
    }
    let params = extract_fmtps(media).into_iter().find(|(f, _)| f == format).map(|(_, p)| p);
    let aligned = params.as_deref().unwrap_or_default().split(';').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("octet-align").then(|| v.trim().to_string())
    });
    Some(aligned.unwrap_or_else(|| "0".to_string()))
}

/// The format of `other` that is the same payload format as `format` of
/// `media`, if any.
fn counterpart(format: &str, media: &MediaLine, other: &MediaLine) -> Option<String> {
    let enc = encoding(format, media);
    let same = |f: &String| {
        if is_dynamic(format) || is_dynamic(f) {
            let (Some(a), Some(b)) = (&enc, encoding(f, other)) else { return false };
            *a == b && amr_alignment(format, media, a) == amr_alignment(f, other, &b)
        } else {
            f == format
        }
    };
    other.formats.iter().find(|f| same(f)).cloned()
}

/// The formats an answered stream keeps: the first common format in the
/// preferred order that is not `telephone-event`, then, on audio, the first
/// common `telephone-event` in that order. `None` where no such first format
/// exists: `telephone-event` alone carries no media.
fn kept_formats(
    offered: &MediaLine,
    own: &MediaLine,
    preference: FormatPreference,
) -> Option<Vec<Kept>> {
    let common: Vec<Kept> = match preference {
        FormatPreference::Offerer => offered
            .formats
            .iter()
            .filter_map(|f| Some(Kept { own: counterpart(f, offered, own)?, offered: f.clone() }))
            .collect(),
        FormatPreference::Answerer => own
            .formats
            .iter()
            .filter_map(|f| Some(Kept { offered: counterpart(f, own, offered)?, own: f.clone() }))
            .collect(),
    };
    let events = |k: &Kept| {
        encoding(&k.offered, offered)
            .is_some_and(|enc| enc.split('/').next() == Some("telephone-event"))
    };
    let codec = common.iter().position(|k| !events(k))?;
    let te = offered
        .r#type
        .eq_ignore_ascii_case("audio")
        .then(|| common.iter().position(events))
        .flatten();
    Some(
        common
            .into_iter()
            .enumerate()
            .filter(|(i, _)| *i == codec || Some(*i) == te)
            .map(|(_, k)| k)
            .collect(),
    )
}

/// The one `crypto` line answering the SDES keys `offered` carries (RFC 4568
/// §5.1.2): `own`'s key for the first suite both hold, in the order
/// `preference` names (`required` alone where given), under the offer's tag.
/// `Some(None)` where the offer carries no SDES; `None` where it does and no
/// suite qualifies.
fn sdes_answer(
    offered: &MediaLine,
    own: &MediaLine,
    preference: FormatPreference,
    required: Option<&str>,
) -> Option<Option<String>> {
    let offered_keys = extract_cryptos(offered);
    if offered_keys.is_empty() {
        return Some(None);
    }
    let own_keys = extract_cryptos(own);
    let ranked: Vec<&String> = match preference {
        FormatPreference::Offerer => offered_keys.iter().map(|(_, s, _)| s).collect(),
        FormatPreference::Answerer => own_keys.iter().map(|(_, s, _)| s).collect(),
    };
    ranked.into_iter().filter(|s| required.is_none_or(|r| s.eq_ignore_ascii_case(r))).find_map(
        |suite| {
            let (tag, suite, _) =
                offered_keys.iter().find(|(_, s, _)| s.eq_ignore_ascii_case(suite))?;
            let (_, _, rest) = own_keys.iter().find(|(_, s, _)| s.eq_ignore_ascii_case(suite))?;
            Some(Some(format!("a=crypto:{tag} {suite} {rest}")))
        },
    )
}

/// Whether `media` (under the `session` lines of its description) is set up
/// over DTLS (RFC 5763/5764): a TLS transport, or a `setup` / `fingerprint`
/// attribute. The handshake role and certificate are the party's own, so no
/// answer on its behalf can state them: such a stream is rejected.
fn over_dtls(media: &MediaLine, session: &str) -> bool {
    media.transport.to_ascii_uppercase().contains("TLS")
        || media.attributes.iter().any(|a| a.starts_with("setup:") || a.starts_with("fingerprint:"))
        || lines(session).any(|l| l.starts_with("a=setup:") || l.starts_with("a=fingerprint:"))
}

/// What one answered stream states.
struct Answered<'a> {
    stream: &'a MediaLine,
    own: &'a MediaLine,
    kept: &'a [Kept],
    crypto: Option<String>,
    dir: SdpDirection,
    inherited: SdpDirection,
}

/// The attributes bound to one format by their first token (RFC 4566 §6, RFC
/// 4585 §4.2).
const PER_FORMAT: [&str; 3] = ["rtpmap:", "fmtp:", "rtcp-fb:"];

/// `own`'s media section `text` answering as `answer` states: the m-line's
/// format list replaced, each per-format attribute kept for a kept format only
/// and renumbered (an `rtpmap` spelled as the offer spells it), the first
/// `crypto` line written as the answer's one and any other dropped, the first
/// direction attribute written as the answer's and any other dropped, the
/// direction added at the end where the section states none and the inherited
/// one differs.
fn answered_section(out: &mut String, text: &str, answer: &Answered<'_>, eol: &str) {
    let mut section = lines(text);
    let m_value = section.next().and_then(|l| l.get(2..)).unwrap_or_default();
    let port_field = m_value.split_whitespace().nth(1).unwrap_or("0");
    let formats: Vec<&str> = answer.kept.iter().map(|k| k.offered.as_str()).collect();
    push_line(
        out,
        &format!(
            "m={} {port_field} {} {}",
            answer.own.r#type,
            answer.own.transport,
            formats.join(" ")
        ),
        eol,
    );
    let offered_maps = extract_rtpmaps(answer.stream);
    let offered_mid = answer.stream.attributes.iter().find_map(|a| a.strip_prefix("mid:"));
    let (mut dir_written, mut crypto_written, mut mid_written) = (false, false, false);
    let mut mapped: Vec<&str> = Vec::new();
    for line in section {
        let attr = line.strip_prefix("a=");
        let bound = attr.and_then(|a| {
            let name = PER_FORMAT.iter().find(|p| a.starts_with(**p))?;
            let rest = &a[name.len()..];
            let (format, tail) = rest.split_once(' ').unwrap_or((rest, ""));
            Some((*name, format, tail))
        });
        if let Some((name, format, tail)) = bound {
            if format == "*" {
                push_line(out, line, eol);
                continue;
            }
            let Some(kept) = answer.kept.iter().find(|k| k.own == format) else { continue };
            if name == "rtpmap:" {
                mapped.push(&kept.offered);
            }
            let tail = match offered_maps.iter().find(|(pt, _)| *pt == kept.offered) {
                Some((_, enc)) if name == "rtpmap:" => enc.as_str(),
                _ => tail,
            };
            push_line(out, format!("a={name}{} {tail}", kept.offered).trim_end(), eol);
            continue;
        }
        if attr.is_some_and(|a| a.starts_with("mid:")) {
            if let (Some(mid), false) = (offered_mid, mid_written) {
                push_line(out, &format!("a=mid:{mid}"), eol);
            }
            mid_written = true;
            continue;
        }
        if attr.is_some_and(|a| a.starts_with("crypto:")) {
            if let (Some(crypto), false) = (&answer.crypto, crypto_written) {
                push_line(out, crypto, eol);
            }
            crypto_written = true;
            continue;
        }
        match attr.and_then(direction_of) {
            Some(_) if dir_written => {}
            Some(_) => {
                push_line(out, &format!("a={}", answer.dir.token()), eol);
                dir_written = true;
            }
            None => push_line(out, line, eol),
        }
    }
    if let (Some(mid), false) = (offered_mid, mid_written) {
        push_line(out, &format!("a=mid:{mid}"), eol);
    }
    for kept in answer.kept.iter().filter(|k| is_dynamic(&k.offered)) {
        if mapped.contains(&kept.offered.as_str()) {
            continue;
        }
        if let Some((_, enc)) = offered_maps.iter().find(|(pt, _)| *pt == kept.offered) {
            push_line(out, &format!("a=rtpmap:{} {enc}", kept.offered), eol);
        }
    }
    if !dir_written && answer.dir != answer.inherited {
        push_line(out, &format!("a={}", answer.dir.token()), eol);
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
