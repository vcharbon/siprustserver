//! The inputs the scaling cases grow: a request whose header count, Via
//! count, Route count or body size is the axis, and the bodies themselves.

use sip_message::{Bytes, CustomParser, SipMessage, SipParser, SipParserLimits, SipRequest};

/// The host every own Route and Record-Route names.
pub const PROXY_HOST: &str = "proxy.example.com";

/// The known headers the filler alternates with extension headers, each legal
/// on any number of lines.
const KNOWN_FILLER: &[&str] = &[
    "Allow: INVITE, ACK, BYE, CANCEL, OPTIONS",
    "Supported: timer, replaces",
    "Accept: application/sdp",
    "User-Agent: scaling-probe/1.0",
    "P-Early-Media: supported",
    "Date: Sat, 13 Nov 2010 23:29:00 GMT",
];

/// What a request is grown on.
#[derive(Clone, Default)]
pub struct Shape {
    /// Total lines of the head, start line included; the filler makes up
    /// whatever the other fields leave.
    pub lines: usize,
    /// Via entries, top first; at least one.
    pub vias: usize,
    /// Via entries per comma-separated line; 0 puts them all on one line.
    pub vias_per_line: usize,
    /// Route entries naming this proxy.
    pub own_routes: usize,
    /// Route entries per comma-separated line; 0 puts them all on one line.
    pub routes_per_line: usize,
    /// Record-Route entries.
    pub record_routes: usize,
    /// The body and its media type.
    pub body: Option<(String, Vec<u8>)>,
}

impl Shape {
    pub fn lines(lines: usize) -> Self {
        Self { lines, vias: 1, vias_per_line: 1, ..Self::default() }
    }

    pub fn vias(vias: usize, per_line: usize) -> Self {
        Self { vias, vias_per_line: per_line, ..Self::default() }
    }

    pub fn own_routes(routes: usize, per_line: usize) -> Self {
        Self {
            vias: 1,
            vias_per_line: 1,
            own_routes: routes,
            routes_per_line: per_line,
            ..Self::default()
        }
    }

    pub fn record_routes(record_routes: usize) -> Self {
        Self { vias: 1, vias_per_line: 1, record_routes, ..Self::default() }
    }

    pub fn body(content_type: &str, body: Vec<u8>) -> Self {
        Self {
            vias: 1,
            vias_per_line: 1,
            body: Some((content_type.to_string(), body)),
            ..Self::default()
        }
    }
}

fn via(i: usize) -> String {
    format!("SIP/2.0/UDP 10.0.{}.{}:5060;branch=z9hG4bK-h{i}", i / 250, i % 250 + 1)
}

/// Writes `entries` as lines of `name`, `per_line` entries to a line (0: all
/// on one); the lines written.
fn push_list(wire: &mut String, name: &str, entries: &[String], per_line: usize) -> usize {
    let per_line = if per_line == 0 { entries.len().max(1) } else { per_line };
    let mut lines = 0;
    for chunk in entries.chunks(per_line) {
        wire.push_str(&format!("{name}: {}\r\n", chunk.join(", ")));
        lines += 1;
    }
    lines
}

/// The INVITE `shape` describes.
pub fn invite(shape: &Shape) -> Bytes {
    let mut wire = String::from("INVITE sip:bob@192.0.2.99:5060 SIP/2.0\r\n");
    let mut lines = 1;
    let vias: Vec<String> = (0..shape.vias.max(1)).map(via).collect();
    lines += push_list(&mut wire, "Via", &vias, shape.vias_per_line);
    wire.push_str("Max-Forwards: 70\r\n");
    let routes: Vec<String> =
        (0..shape.own_routes).map(|i| format!("<sip:{PROXY_HOST};r={i};lr>")).collect();
    lines += push_list(&mut wire, "Route", &routes, shape.routes_per_line);
    let record_routes: Vec<String> =
        (0..shape.record_routes).map(|i| format!("<sip:rr{i}.example.net;lr>")).collect();
    lines += push_list(&mut wire, "Record-Route", &record_routes, 1);
    wire.push_str(
        "From: Alice <sip:alice@example.com>;tag=1928\r\n\
         To: Bob <sip:bob@example.com>\r\n\
         Call-ID: a84b4c76e66710@pc33.example.com\r\n\
         CSeq: 314159 INVITE\r\n\
         Contact: <sip:alice@10.0.0.1:5060>\r\n",
    );
    lines += 6;
    let mut k = 0;
    while lines + 1 < shape.lines {
        if k % 2 == 0 {
            wire.push_str(&format!("X-Filler-{k}: value-{k}\r\n"));
        } else {
            wire.push_str(KNOWN_FILLER[(k / 2) % KNOWN_FILLER.len()]);
            wire.push_str("\r\n");
        }
        k += 1;
        lines += 1;
    }
    let body = match &shape.body {
        Some((content_type, body)) => {
            wire.push_str(&format!("Content-Type: {content_type}\r\n"));
            body.clone()
        }
        None => Vec::new(),
    };
    wire.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    let mut bytes = wire.into_bytes();
    bytes.extend_from_slice(&body);
    Bytes::from(bytes)
}

/// The parser every case reads with: the production grammar, its line and
/// length bounds raised so an axis can run past them. The bounds are policy;
/// the work a parse does per line is what the cases measure.
pub fn parser() -> CustomParser {
    CustomParser::with_limits(SipParserLimits {
        max_header_lines: 100_000,
        max_header_length: 1 << 20,
        ..SipParserLimits::default()
    })
}

/// `wire` parsed as a request.
pub fn request(wire: &Bytes) -> SipRequest {
    match parser().parse_shared(wire.clone()).expect("the fixture parses") {
        SipMessage::Request(request) => request,
        SipMessage::Response(_) => panic!("the fixture is a request"),
    }
}

/// An SDP offer of about `size` bytes grown by media sections.
pub fn sdp_sections(size: usize) -> Vec<u8> {
    let mut sdp = String::from(
        "v=0\r\no=alice 2890844526 2890844526 IN IP4 192.0.2.10\r\ns=-\r\n\
         c=IN IP4 192.0.2.10\r\nt=0 0\r\n",
    );
    let mut port = 40000u32;
    while sdp.len() < size {
        sdp.push_str(&format!(
            "m=audio {port} RTP/AVP 0 8 96\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:8 PCMA/8000\r\n\
             a=rtpmap:96 opus/48000/2\r\na=sendrecv\r\n"
        ));
        port += 2;
    }
    sdp.into_bytes()
}

/// An SDP offer of about `size` bytes grown by attribute lines of its one
/// audio section.
pub fn sdp_attributes(size: usize) -> Vec<u8> {
    let mut sdp = String::from(
        "v=0\r\no=alice 2890844526 2890844526 IN IP4 192.0.2.10\r\ns=-\r\n\
         c=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 40000 RTP/AVP 0 8 96\r\n\
         a=rtpmap:0 PCMU/8000\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:96 opus/48000/2\r\n",
    );
    let mut k = 0;
    while sdp.len() < size {
        sdp.push_str(&format!("a=x-probe:{k}\r\n"));
        k += 1;
    }
    sdp.push_str("a=sendrecv\r\n");
    sdp.into_bytes()
}

/// The boundary of every multipart body here.
pub const BOUNDARY: &str = "unique-boundary-1";

/// A `multipart/mixed` body of about `size` bytes: an SDP part, then parts of
/// opaque text.
pub fn multipart(size: usize) -> Vec<u8> {
    let sdp = sdp_sections(0);
    let mut body = format!("--{BOUNDARY}\r\nContent-Type: application/sdp\r\n\r\n").into_bytes();
    body.extend_from_slice(&sdp);
    while body.len() < size {
        body.extend_from_slice(
            format!(
                "\r\n--{BOUNDARY}\r\nContent-Type: text/plain\r\n\r\n{}",
                "opaque-content-line-".repeat(10)
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

/// The media type of [`multipart`].
pub fn multipart_type() -> String {
    format!("multipart/mixed;boundary={BOUNDARY}")
}
