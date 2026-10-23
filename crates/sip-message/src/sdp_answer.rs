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
    direction_of, extract_cryptos, extract_direction, extract_rtpmaps, parse_sdp_body, MediaLine,
    SdpDirection, Sections,
};

mod formats;

use lookup_map::LookupMap;

use formats::{is_dynamic, FormatTable};

/// Whose order of preference picks the one format an answered stream keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatPreference {
    /// The first of the offer's formats the own description also lists.
    Offerer,
    /// The first of the own description's formats the offer also lists.
    Answerer,
}

/// The answer to `offer` built out of `own`, a description the answering party
/// stated earlier, or `None` where either is not a session description or
/// either groups streams under BUNDLE (RFC 8843: out of scope, the shared
/// transport and its demultiplexing are the party's own; the caller refuses
/// the offer instead).
///
/// Session-level lines are `own`'s, its `o=` value replaced by `origin` where
/// one is given. The answer has one m-line per offered stream, in order,
/// matched to `own`'s stream of the same rank:
///
/// - an offered stream with port 0 is answered with its own m-line;
/// - one `own` does not describe at that rank, or describes with another media
///   type or transport profile, or with no format in common, or offers SDES
///   keys (RFC 4568) with no suite in common — keys on one side only included
///   —, or either side sets up a connection of its own (DTLS, RFC 5763;
///   connection-oriented media over TCP, RFC 4145 / RFC 4975; any `setup`
///   attribute, whatever the transport: out of scope, the role and
///   certificate are the party's own), is answered rejected: its m-line with
///   port 0 and the offer's `mid`;
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

/// [`answer_from_own`] agreeing with `agreed`, the other answer of the same
/// exchange (one m-line per stream of the same rank), so both ends of a
/// back-to-back exchange run the same streams under one SRTP context: a stream
/// `agreed` rejects at a rank is rejected here, and a stream `agreed` keeps an
/// SDES suite for keeps that suite. A rank `agreed` does not describe chooses
/// as [`answer_from_own`] does.
pub fn answer_from_own_agreeing(
    offer: &[u8],
    own: &[u8],
    preference: FormatPreference,
    origin: Option<&str>,
    agreed: Option<&[u8]>,
) -> Option<Vec<u8>> {
    let agreed_media = agreed.and_then(parse_sdp_body).map(|d| d.media).unwrap_or_default();
    let offer_doc = parse_sdp_body(offer)?;
    let own_doc = parse_sdp_body(own)?;
    let offer_text = String::from_utf8_lossy(offer);
    let own_text = String::from_utf8_lossy(own);
    let offered = Sections::of(&offer_text)?;
    let owned = Sections::of(&own_text)?;
    if bundles(offered.session) || bundles(owned.session) {
        return None;
    }
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
    for (i, stream) in offer_doc.media.iter().enumerate() {
        let offered_value = offered.media.get(i)?.value();
        // A rejected m-line keeps the offer's `mid` (RFC 5888 §9.2).
        let mid = stream.attributes.iter().find_map(|a| a.strip_prefix("mid:"));
        let reject = |out: &mut String, m_value: String| {
            push_line(out, &format!("m={m_value}"), eol);
            if let Some(c) = &stub_c {
                push_line(out, &format!("c={c}"), eol);
            }
            if let Some(mid) = mid {
                push_line(out, &format!("a=mid:{mid}"), eol);
            }
        };
        let agreed = agreed_media.get(i);
        if agreed.is_some_and(|m| m.port == Some(0)) && stream.port != Some(0) {
            reject(&mut out, with_port_zero(offered_value));
            continue;
        }
        if stream.port == Some(0) {
            reject(&mut out, offered_value.to_string());
            continue;
        }
        let Some(mine) = own_doc.media.get(i).filter(|m| m.r#type == stream.r#type) else {
            reject(&mut out, with_port_zero(offered_value));
            continue;
        };
        if mine.port == Some(0) {
            reject(&mut out, owned.media.get(i)?.value().to_string());
            continue;
        }
        let answerable = mine.transport == stream.transport
            && !sets_up_own_connection(stream, offered.session)
            && !sets_up_own_connection(mine, owned.session);
        let kept = answerable.then(|| kept_formats(stream, mine, preference)).flatten();
        let required =
            agreed.and_then(|m| extract_cryptos(m).into_iter().next()).map(|(_, s, _)| s);
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
        answered_section(&mut out, owned.media.get(i)?.text, &answer, eol);
    }
    Some(out.into_bytes())
}

/// The two answers a back-to-back UA composes for a re-offer inside an early
/// dialog it answers on the first offerer's behalf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BothWays {
    /// The re-offer answered out of the offer the re-offerer was sent.
    pub to_reofferer: Vec<u8>,
    /// The first offerer's own offer answered out of the re-offer.
    pub to_offerer: Vec<u8>,
}

/// `reoffer` answered both ways ([`BothWays`]), or `None` where it cannot be
/// answered and the re-offerer is refused (RFC 3311 §5.2): either answer
/// cannot be built ([`answer_from_own_agreeing`]), or no stream of a re-offer
/// with a live one is accepted.
///
/// The re-offerer is answered out of `sent`, the offer it was sent, in its
/// own order of preference, under `origin` (the next version of that
/// session); the first offerer's `original` offer is answered out of the
/// re-offer in the re-offerer's order, under the re-offer's origin. Both
/// answers accept the same streams under one SDES suite: the re-offerer's
/// answer is built, the first offerer's agrees with it, and the re-offerer's
/// is built again agreeing with that ([`answer_from_own_agreeing`]).
pub fn answer_reoffer_both_ways(
    reoffer: &[u8],
    sent: &[u8],
    original: &[u8],
    origin: &str,
) -> Option<BothWays> {
    let to_reofferer = |agreed: Option<&[u8]>| {
        answer_from_own_agreeing(reoffer, sent, FormatPreference::Offerer, Some(origin), agreed)
    };
    let first = to_reofferer(None)?;
    let to_offerer = answer_from_own_agreeing(
        original,
        reoffer,
        FormatPreference::Answerer,
        None,
        Some(&first),
    )?;
    let to_reofferer = to_reofferer(Some(&to_offerer))?;
    if has_live_stream(reoffer) && !has_live_stream(&to_reofferer) {
        return None;
    }
    Some(BothWays { to_reofferer, to_offerer })
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
    let sections = Sections::of(&text)?;
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

/// The formats an answered stream keeps: the first common format in the
/// preferred order that is not `telephone-event`, then, on audio, the first
/// common `telephone-event` in that order. `None` where no such first format
/// exists: `telephone-event` alone carries no media.
fn kept_formats(
    offered: &MediaLine,
    own: &MediaLine,
    preference: FormatPreference,
) -> Option<Vec<Kept>> {
    let offered_table = FormatTable::of(offered);
    let own_table = FormatTable::of(own);
    let common: Vec<Kept> = match preference {
        FormatPreference::Offerer => offered
            .formats
            .iter()
            .filter_map(|f| {
                Some(Kept { own: own_table.counterpart(f, &offered_table)?, offered: f.clone() })
            })
            .collect(),
        FormatPreference::Answerer => own
            .formats
            .iter()
            .filter_map(|f| {
                Some(Kept { offered: offered_table.counterpart(f, &own_table)?, own: f.clone() })
            })
            .collect(),
    };
    let events = |k: &Kept| {
        offered_table
            .encoding(&k.offered)
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
/// `Some(None)` where neither side carries SDES; `None` where either does and
/// no suite qualifies (keys on one side only included, whichever side).
fn sdes_answer(
    offered: &MediaLine,
    own: &MediaLine,
    preference: FormatPreference,
    required: Option<&str>,
) -> Option<Option<String>> {
    let offered_keys = extract_cryptos(offered);
    let own_keys = extract_cryptos(own);
    if offered_keys.is_empty() && own_keys.is_empty() {
        return Some(None);
    }
    let ranked: Vec<&String> = match preference {
        FormatPreference::Offerer => offered_keys.iter().map(|(_, s, _)| s).collect(),
        FormatPreference::Answerer => own_keys.iter().map(|(_, s, _)| s).collect(),
    };
    let offered_by_suite = first_by_suite(&offered_keys);
    let own_by_suite = first_by_suite(&own_keys);
    ranked.into_iter().filter(|s| required.is_none_or(|r| s.eq_ignore_ascii_case(r))).find_map(
        |suite| {
            let folded = suite.to_ascii_lowercase();
            let (tag, suite, _) = offered_by_suite.get(&folded)?;
            let (_, _, rest) = own_by_suite.get(&folded)?;
            Some(Some(format!("a=crypto:{tag} {suite} {rest}")))
        },
    )
}

/// The first key of each crypto suite `of` the keys, by its lower-cased name
/// (suite names compare case-insensitively).
fn first_by_suite(of: &[(String, String, String)]) -> LookupMap<String, &(String, String, String)> {
    let mut first = LookupMap::new();
    for key in of {
        first.entry(key.1.to_ascii_lowercase()).or_insert(key);
    }
    first
}

/// Whether `media` (under the `session` lines of its description) sets up a
/// connection of its own: over DTLS (RFC 5763/5764: a TLS or DTLS transport
/// component, or a `fingerprint`), or connection-oriented media (RFC 4145 comedia, RFC 4975
/// MSRP: TCP as any component of the transport — TCP/RTP/AVP, RTP/AVP/TCP,
/// TCP/BFCP, TCP/MSRP, TCP/TLS/… — or a `setup` attribute, whatever the
/// transport). The connection role and
/// certificate are the party's own, so no answer on its behalf can state
/// them: such a stream is rejected.
fn sets_up_own_connection(media: &MediaLine, session: &str) -> bool {
    let transport = &media.transport;
    transport.split('/').any(|p| ["TCP", "TLS", "DTLS"].iter().any(|t| p.eq_ignore_ascii_case(t)))
        || media.attributes.iter().any(|a| a.starts_with("setup:") || a.starts_with("fingerprint:"))
        || lines(session).any(|l| l.starts_with("a=setup:") || l.starts_with("a=fingerprint:"))
}

/// Whether session-level `lines` group streams under BUNDLE (RFC 8843).
fn bundles(session: &str) -> bool {
    lines(session).any(|l| {
        l.strip_prefix("a=group:").is_some_and(|g| {
            g.split_whitespace().next().is_some_and(|s| s.eq_ignore_ascii_case("BUNDLE"))
        })
    })
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
    let offered_maps: LookupMap<String, String> =
        extract_rtpmaps(answer.stream).into_iter().collect();
    let offered_mid = answer.stream.attributes.iter().find_map(|a| a.strip_prefix("mid:"));
    let (mut dir_written, mut crypto_written, mut mid_written) = (false, false, false);
    let mut mapped: Vec<&str> = Vec::new();
    for line in section {
        let attr = line.strip_prefix("a=");
        let bound = attr.and_then(|a| {
            let (name, rest) = PER_FORMAT.iter().find_map(|p| Some((*p, a.strip_prefix(*p)?)))?;
            let (format, tail) = rest.split_once(' ').unwrap_or((rest, ""));
            Some((name, format, tail))
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
            let tail = match offered_maps.get(&kept.offered) {
                Some(enc) if name == "rtpmap:" => enc.as_str(),
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
        if let Some(enc) = offered_maps.get(&kept.offered) {
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
