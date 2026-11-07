//! Message bodies and session descriptions. `sdp()` is the single decision point
//! of an offer: a row states what it returns on a valid body, or that ingress
//! refuses the body's framing. The description rows state what RFC 8866 and
//! RFC 3264 say of a value.

use sip_message::sdp::validate_offer_answer_body;
use sip_message::{
    decompose_multipart, extract_direction, parse_origin, parse_sdp_body, sdp_media_equivalent,
    stamp_session, write_check, LocatedPart, SdpDirection, SipMessage,
};

use super::break_rule::refused;
use super::message::{read, Template};
use super::ratchet::{settle, Row};

const SDP: &str = "v=0\r\no=- 1 1 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\n\
                   m=audio 49170 RTP/AVP 0\r\n";

/// An INVITE carrying `body` under `content_type`, and any further lines.
fn invite(content_type: &str, lines: &[&str], body: impl Into<Vec<u8>>) -> Template {
    let template = Template::request("INVITE").add(format!("Content-Type: {content_type}"));
    lines.iter().fold(template, |t, line| t.add(*line)).body(body)
}

fn sdp_of(raw: &[u8]) -> Result<Option<Vec<u8>>, String> {
    match read(raw) {
        Ok(SipMessage::Request(r)) => Ok(r.sdp().map(<[u8]>::to_vec)),
        Ok(SipMessage::Response(r)) => Ok(r.sdp().map(<[u8]>::to_vec)),
        Err(e) => Err(e.reason),
    }
}

/// `sdp()` of the message reads the session description.
fn offers(name: &str, template: Template) -> Row {
    let outcome = sdp_of(&template.bytes()).and_then(|sdp| match sdp {
        Some(found) if found == SDP.as_bytes() => Ok(()),
        Some(found) => Err(format!("read {:?}", String::from_utf8_lossy(&found))),
        None => Err("no offer".to_string()),
    });
    Row::new(format!("body/{name}"), outcome)
}

/// `sdp()` of the message reads no session description.
fn offers_nothing(name: &str, template: Template) -> Row {
    let outcome = sdp_of(&template.bytes()).and_then(|sdp| match sdp {
        None => Ok(()),
        Some(found) => Err(format!("read {:?}", String::from_utf8_lossy(&found))),
    });
    Row::new(format!("body/{name}"), outcome)
}

fn answered(name: &str, template: Template) -> Row {
    let because = Some("content-type|boundary|multipart|body");
    Row::new(format!("body/{name}"), refused(&template.bytes(), true, because))
}

fn mixed(parts: &str) -> Template {
    invite("multipart/mixed;boundary=b", &[], parts)
}

fn sdp_part() -> String {
    format!("Content-Type: application/sdp\r\n\r\n{SDP}")
}

fn body_rows() -> Vec<Row> {
    let part = sdp_part();
    let long = "b".repeat(80);
    vec![
        // A body that frames its description as RFC 2046 §5.1 and RFC 5621 say.
        offers("bare-description", invite("application/sdp", &[], SDP)),
        offers("one-part", mixed(&format!("--b\r\n{part}\r\n--b--\r\n"))),
        offers("preamble", mixed(&format!("a preamble\r\n--b\r\n{part}\r\n--b--\r\n"))),
        offers("head-starts-with-a-blank-line", mixed(&format!("\r\n--b\r\n{part}\r\n--b--\r\n"))),
        offers("padding-on-the-delimiter", mixed(&format!("--b  \t\r\n{part}\r\n--b--\r\n"))),
        offers("session-required-on-the-multipart", invite(
            "multipart/mixed;boundary=b",
            &["Content-Disposition: session;handling=required"],
            format!("--b\r\n{part}\r\n--b--\r\n"),
        )),
        offers("session-part", mixed(&format!("--b\r\nContent-Disposition: session;handling=required\r\n{part}\r\n--b--\r\n"))),
        Row::new("body/no-closing-delimiter".to_string(), last_part_kept()),
        // RFC 3261 §25.1: SLASH = SWS "/" SWS.
        offers("spaces-around-the-slash", invite("application / sdp", &[], SDP)),
        offers("media-type-case", invite("APPLICATION/SDP", &[], SDP)),
        // RFC 2046 §5.1.1: a delimiter is a line.
        Row::new("body/delimiter-text-inside-a-line".to_string(), inside_a_line()),
        Row::new("body/delimiter-prefix-of-a-line".to_string(), prefix_of_a_line()),
        offers_nothing("delimiter-line-ends-in-crlf", mixed(&format!("--b\nContent-Type: application/sdp\n\n{SDP}\n--b--\n"))),
        offers_nothing("description-in-the-epilogue", mixed(&format!("--b\r\nContent-Type: text/plain\r\n\r\nhi\r\n--b--\r\n--b\r\n{part}"))),
        // RFC 2045 §3, RFC 2046 §5.1: a part's head.
        offers("folded-part-header", mixed(&format!("--b\r\nContent-Type:\r\n application/sdp\r\n\r\n{SDP}\r\n--b--\r\n"))),
        offers("folded-part-subtype", mixed(&format!("--b\r\nContent-Type: application/\r\n sdp\r\n\r\n{SDP}\r\n--b--\r\n"))),
        offers("part-content-length", mixed(&format!("--b\r\nContent-Length: {}\r\n{part}\r\n--b--\r\n", SDP.len()))),
        offers_nothing("continuation-is-no-header", mixed(&format!("--b\r\nX-Note: first\r\n Content-Type: application/sdp\r\n\r\n{SDP}\r\n--b--\r\n"))),
        offers_nothing("part-with-no-headers-is-text", mixed(&format!("--b\r\n\r\nContent-Type: application/sdp\r\n\r\n{SDP}\r\n--b--\r\n"))),
        Row::new("body/part-with-no-headers-keeps-its-content".to_string(), part_content("--b\r\n\r\nline one\r\n\r\nline: three\r\n--b--\r\n", "line one\r\n\r\nline: three")),
        Row::new("body/part-with-headers-only-keeps-its-type".to_string(), part_type("--b\r\nContent-Type: application/x\r\nContent-ID: <p1>\r\n--b--\r\n", "application/x")),
        // RFC 5621 §4.2, §6.1: nesting and alternatives.
        offers("nested-alternative", mixed(&format!("--b\r\nContent-Type: multipart/alternative;boundary=c\r\n\r\n--c\r\nContent-Type: text/plain\r\n\r\nhi\r\n--c\r\n{part}\r\n--c--\r\n\r\n--b--\r\n"))),
        offers("last-of-an-alternative", invite(
            "multipart/alternative;boundary=b",
            &[],
            format!("--b\r\nContent-Type: application/sdp\r\n\r\nv=0\r\n\r\n--b\r\n{part}\r\n--b--\r\n"),
        )),
        offers("nesting-at-the-depth-cap", nested(4)),
        offers_nothing("nesting-past-the-depth-cap", nested(5)),
        // RFC 5621 §8.2: a description's disposition.
        offers_nothing("bare-early-session", invite("application/sdp", &["Content-Disposition: early-session"], SDP)),
        offers_nothing("bare-render", invite("application/sdp", &["Content-Disposition: render"], SDP)),
        offers_nothing("early-session-part", mixed(&format!("--b\r\nContent-Disposition: early-session\r\n{part}\r\n--b--\r\n"))),
        offers("session-after-an-early-session-part", mixed(&format!("--b\r\nContent-Disposition: early-session\r\nContent-Type: application/sdp\r\n\r\n{}\r\n--b\r\n{part}\r\n--b--\r\n", SDP.replace("49170", "40000")))),
        offers("session-on-a-bare-description", invite("application/sdp", &["Content-Disposition: session;handling=required"], SDP)),
        // RFC 3261 §20.15, RFC 2046 §5.1.1: ingress refuses what no reader frames.
        answered("no-content-type", Template::request("INVITE").body(SDP)),
        answered("content-type-with-no-subtype", invite("application", &[], SDP)),
        answered("multipart-no-boundary", invite("multipart/mixed", &[], format!("--b\r\n{part}\r\n--b--\r\n"))),
        answered("multipart-boundary-with-no-value", invite("multipart/mixed;boundary", &[], format!("--b\r\n{part}\r\n--b--\r\n"))),
        answered("multipart-boundary-of-80-bytes", invite(&format!("multipart/mixed;boundary={long}"), &[], format!("--{long}\r\n{part}\r\n--{long}--\r\n"))),
        answered("multipart-boundary-ending-in-a-space", invite("multipart/mixed;boundary=\"b \"", &[], format!("--b \r\n{part}\r\n--b --\r\n"))),
        answered("multipart-empty-boundary", invite("multipart/mixed;boundary=\"\"", &[], format!("--\r\n{part}\r\n----\r\n"))),
        answered("multipart-boundary-with-a-comma", invite("multipart/mixed;boundary=a,b", &[], format!("--a,b\r\n{part}\r\n--a,b--\r\n"))),
        answered("multipart-two-boundaries", invite("multipart/mixed;boundary=a;boundary=b", &[], format!("--a\r\n{part}\r\n--a--\r\n"))),
        // RFC 3261 §20.11: the disposition decides whether a body is a session's.
        Row::new("body/unreadable-disposition".to_string(), refused(&invite("application/sdp", &["Content-Disposition: session;;"], SDP).bytes(), true, Some("content-disposition"))),
        Row::new("body/empty-disposition".to_string(), refused(&invite("application/sdp", &["Content-Disposition:"], SDP).bytes(), true, Some("content-disposition"))),
        answered("multipart-unquoted-non-token-boundary", invite("multipart/mixed;boundary=----=_Part_1", &[], format!("------=_Part_1\r\n{part}\r\n------=_Part_1--\r\n"))),
    ]
}

/// The last part of a body that never closes keeps its content, up to the CRLF
/// that may or may not begin a delimiter.
fn last_part_kept() -> Result<(), String> {
    let body = format!("--b\r\n{}", sdp_part());
    match sdp_of(&mixed(&body).bytes())? {
        Some(found) if found.trim_ascii_end() == SDP.as_bytes().trim_ascii_end() => Ok(()),
        other => Err(format!("read {other:?}")),
    }
}

/// The one part of `body` under boundary `b`, as `decompose` reads it.
fn the_part(body: &str) -> Result<LocatedPart, String> {
    let parts = decompose_multipart(body.as_bytes(), "b");
    let mut parts = parts.into_iter();
    parts.next().ok_or("no part".to_string())
}

fn part_content(body: &str, content: &str) -> Result<(), String> {
    let part = the_part(body)?;
    let found = &body.as_bytes()[part.offset..part.offset + part.len];
    let found = String::from_utf8_lossy(found).into_owned();
    (found == content).then_some(()).ok_or(format!("content {found:?}"))
}

fn part_type(body: &str, content_type: &str) -> Result<(), String> {
    let part = the_part(body)?;
    (part.content_type == content_type).then_some(()).ok_or(format!("type {:?}", part.content_type))
}

/// A line of the description holding the delimiter's text ends no part.
fn inside_a_line() -> Result<(), String> {
    let description = format!("{SDP}a=x:prefix--b-suffix\r\n");
    let body = format!("--b\r\nContent-Type: application/sdp\r\n\r\n{description}\r\n--b--\r\n");
    match sdp_of(&mixed(&body).bytes())? {
        Some(found) if found == description.as_bytes() => Ok(()),
        other => Err(format!("read {:?}", other.map(|f| String::from_utf8_lossy(&f).into_owned()))),
    }
}

/// `--bb` is a line of its part under boundary `b`.
fn prefix_of_a_line() -> Result<(), String> {
    let description = format!("{SDP}--bb\r\n");
    let body = format!("--b\r\nContent-Type: application/sdp\r\n\r\n{description}\r\n--b--\r\n");
    match sdp_of(&mixed(&body).bytes())? {
        Some(found) if found == description.as_bytes() => Ok(()),
        other => Err(format!("read {:?}", other.map(|f| String::from_utf8_lossy(&f).into_owned()))),
    }
}

/// A description under `depth` nested multiparts.
fn nested(depth: usize) -> Template {
    let mut inner = sdp_part();
    for level in (0..depth).rev() {
        let boundary = format!("b{level}");
        inner = format!(
            "Content-Type: multipart/mixed;boundary={boundary}\r\n\r\n\
             --{boundary}\r\n{inner}\r\n--{boundary}--\r\n"
        );
    }
    let (head, body) = inner.split_once("\r\n\r\n").expect("a head and a body");
    let content_type = head.trim_start_matches("Content-Type: ").to_string();
    invite(&content_type, &[], body)
}

/// `body` with `from` written as `to`.
fn edited(from: &str, to: &str) -> String {
    SDP.replace(from, to)
}

fn verdict(row: &str, holds: bool, what: &str) -> Row {
    let outcome = if holds { Ok(()) } else { Err(what.to_string()) };
    Row::new(format!("sdp/{row}"), outcome)
}

fn description_rows() -> Vec<Row> {
    let session_direction = SDP.replace("t=0 0\r\n", "t=0 0\r\na=sendonly\r\n");
    let direction = parse_sdp_body(session_direction.as_bytes())
        .and_then(|doc| doc.media.first().map(extract_direction));
    let moved = edited("c=IN IP4 192.0.2.10", "c=IN IP4 192.0.2.99");
    let media_level = "v=0\r\no=- 1 1 IN IP4 192.0.2.10\r\ns=-\r\nt=0 0\r\n\
                       m=audio 49170 RTP/AVP 0\r\nc=IN IP4 192.0.2.10\r\n";
    let port_count = edited("m=audio 49170 RTP/AVP 0", "m=video 49170/2 RTP/AVP 31");
    let ptime = format!("{SDP}a=ptime:2.5\r\n");
    let late_origin = format!("{SDP}o=- 2 2 IN IP4 192.0.2.10\r\n");
    let long_origin = edited("o=- 1 1 IN IP4 192.0.2.10", "o=Sip Express 11 22 IN IP4 192.0.2.10");
    let version = edited("v=0", "v=01");
    let origin_after_media = "v=0\r\ns=-\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n\
                              o=- 1 1 IN IP4 192.0.2.10\r\n";
    let latin1 = edited("s=-", "s=caf\u{e9}").replace('\u{e9}', "\u{0}");
    let latin1: Vec<u8> = latin1.bytes().map(|b| if b == 0 { 0xe9 } else { b }).collect();
    vec![
        // RFC 8866 §5.14, §6: values the audit's reader takes.
        verdict(
            "8866-5.14-media-port-count",
            validate_offer_answer_body(port_count.as_bytes()).is_ok(),
            "port/number refused",
        ),
        verdict(
            "8866-6-ptime-decimal",
            validate_offer_answer_body(ptime.as_bytes()).is_ok(),
            "a decimal ptime refused",
        ),
        verdict(
            "8866-5-origin-after-media-refused",
            validate_offer_answer_body(late_origin.as_bytes()).is_err(),
            "an o= after m= passes",
        ),
        verdict(
            "8866-5-session-direction-inherited",
            direction == Some(SdpDirection::SendOnly),
            "the media reads no session direction",
        ),
        // RFC 8866 §5: session-level values are the default of each media.
        verdict(
            "8866-5-session-address-moved",
            !sdp_media_equivalent(SDP.as_bytes(), moved.as_bytes()),
            "a moved session-level address reads as equal",
        ),
        verdict(
            "8866-5-address-at-either-level",
            sdp_media_equivalent(SDP.as_bytes(), media_level.as_bytes()),
            "one address at two levels reads as different",
        ),
        // RFC 8866 §5.2, §5.1: an origin has six fields, the version is 0.
        verdict(
            "8866-5.2-origin-of-seven-fields",
            parse_origin(long_origin.as_bytes()).is_none(),
            "seven fields read",
        ),
        verdict(
            "8866-5.1-version-with-a-leading-zero",
            parse_origin(version.as_bytes()).is_none(),
            "v=01 reads",
        ),
        verdict(
            "8866-5-origin-only-before-media",
            parse_origin(origin_after_media.as_bytes()).is_none(),
            "an o= after m= reads",
        ),
        // RFC 8866 §5.3: a session name is octets, and a restatement keeps them.
        verdict(
            "8866-5.3-rewrite-keeps-non-utf8",
            stamp_session(&latin1, 7).is_none_or(|stamped| stamped.contains(&0xe9)),
            "a byte became U+FFFD",
        ),
        // A statement the wire would refuse does not leave.
        verdict(
            "stated-value-ingress-refuses",
            write_check::header_line("Expires", "soon").is_err(),
            "write_check passes it",
        ),
    ]
}

#[test]
fn every_body_reads_as_its_rfc_says() {
    settle("body", &body_rows());
}

#[test]
fn every_description_reads_as_its_rfc_says() {
    settle("sdp", &description_rows());
}
