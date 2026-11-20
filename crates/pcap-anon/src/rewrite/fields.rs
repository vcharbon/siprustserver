//! The protocol counters of a datagram, named by field: the digit runs the
//! rewrite keeps, harvest never takes for an identity and the leak check never
//! counts — framing and timing, not identity. A run is judged by the line it
//! stands on:
//!
//! - a header line whose field is a counter or a clock (`CSeq`, `RSeq`,
//!   `RAck`, `Content-Length`, `Max-Forwards`, `Expires`, `Min-Expires`,
//!   `Session-Expires`, `Min-SE`, `Retry-After`, `Timestamp`, `Date`,
//!   `Subscription-State`, compact forms included), whole;
//! - in any header value, the value of an `expires`, `rport`, `ttl`,
//!   `retry-after` or `id` (an event subscription's) parameter;
//! - an SDP line of the media stack (`v=`, `c=`, `t=`, `r=`, `z=`, `m=`,
//!   `b=`, `k=`) and an attribute the media stack defines ([`MEDIA_ATTRIBUTES`]),
//!   whole; the `o=` sess-id and sess-version ([`super::origin_field`]);
//! - anywhere, the port of a hostport (RFC 3261 §25.1: a host name with a
//!   letter, an IPv4 address or a bracketed IPv6 reference, then `:port`)
//!   where a host stands: opening the text (a socket), behind a userinfo, a
//!   SIP or URL scheme, an authority's `//`, a Via sent-protocol or a
//!   Warning code — never a `label:digits` elsewhere.

/// Header fields that are counters or clocks, lowercase, compact forms
/// included (`l`, `x`).
const COUNTER_HEADERS: &[&str] = &[
    "cseq",
    "rseq",
    "rack",
    "content-length",
    "l",
    "max-forwards",
    "expires",
    "min-expires",
    "session-expires",
    "x",
    "min-se",
    "retry-after",
    "timestamp",
    "date",
    "subscription-state",
];

/// Header parameters whose value is a counter, a port or a time.
const COUNTER_PARAMS: &[&str] = &["expires", "rport", "ttl", "retry-after", "id"];

/// SDP attributes the media stack defines: payload formats, ports, stream
/// and candidate ids, keys and fingerprints, timing.
pub(crate) const MEDIA_ATTRIBUTES: &[&str] = &[
    "rtpmap",
    "fmtp",
    "ptime",
    "maxptime",
    "rtcp",
    "rtcp-fb",
    "rtcp-mux",
    "ssrc",
    "ssrc-group",
    "candidate",
    "remote-candidates",
    "ice-ufrag",
    "ice-pwd",
    "crypto",
    "fingerprint",
    "setup",
    "framerate",
    "extmap",
    "sctp-port",
    "sctpmap",
    "max-message-size",
    "mid",
    "msid",
    "group",
    "silencesupp",
    "imageattr",
    "t38faxversion",
    "t38maxbitrate",
    "t38faxmaxbuffer",
    "t38faxmaxdatagram",
    "t38faxratemanagement",
    "t38faxudpec",
    "sendrecv",
    "sendonly",
    "recvonly",
    "inactive",
];

/// URI schemes, which a `:` follows without a port.
const SCHEMES: &[&str] =
    &["sip", "sips", "tel", "urn", "http", "https", "mailto", "im", "pres", "cid", "mid"];

/// Whether the digit run `input[start..end]` is a protocol counter (module
/// doc), an `o=` sess-id or sess-version ([`super::origin_field`]) among them.
pub(crate) fn is_counter(input: &[u8], start: usize, end: usize) -> bool {
    if is_port(input, start, end) {
        return true;
    }
    let line = line_at(input, start);
    if line.len() >= 2 && line[0].is_ascii_lowercase() && line[1] == b'=' {
        // An origin's session id and version are counters whatever they spell.
        return sdp_counter(line)
            || (line[0] == b'o' && super::origin_field(input, start, end).is_some());
    }
    header_name(input, start).is_some_and(|name| COUNTER_HEADERS.contains(&name.as_str()))
        || is_counter_param(input, start)
}

/// Where a response's status code ends (`SIP/2.0 487`, RFC 3261 §7.2), or 0
/// for any other datagram: the bytes before it are protocol no pass rewrites.
pub(crate) fn status_code_end(input: &[u8]) -> usize {
    let code = input.get(8..11).is_some_and(|c| c.iter().all(u8::is_ascii_digit));
    if input.starts_with(b"SIP/2.0 ") && code {
        11
    } else {
        0
    }
}

/// The reason phrase of every status line of `input` — a response's own and
/// a sipfrag body's — from behind the status code to the line's end: the
/// phrase is protocol text ([`super::protocol_spans`]). No header and no SDP
/// line opens with `SIP/2.0 `.
pub(crate) fn reason_phrases(input: &[u8]) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < input.len() {
        let rest = &input[at..];
        let line = rest.iter().position(|b| matches!(b, b'\r' | b'\n')).unwrap_or(rest.len());
        let code = status_code_end(rest);
        if code > 0 && code <= line {
            out.push(at + code..at + line);
        }
        at += line + 1;
    }
    out
}

/// The line `input[at]` stands on, its line break left out.
pub(crate) fn line_at(input: &[u8], at: usize) -> &[u8] {
    let line_start = input[..at].iter().rposition(|b| *b == b'\n').map_or(0, |p| p + 1);
    let line = &input[line_start..];
    &line[..line.iter().position(|b| matches!(b, b'\r' | b'\n')).unwrap_or(line.len())]
}

/// The name, lowercase, of the header field whose line `input[at]` stands
/// on, if it is a header line (`Name: value`).
pub(crate) fn header_name(input: &[u8], at: usize) -> Option<String> {
    let line = line_at(input, at);
    let colon = line.iter().position(|b| *b == b':')?;
    let name = line[..colon].trim_ascii_end();
    let token = !name.is_empty() && name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-');
    token.then(|| String::from_utf8_lossy(name).to_ascii_lowercase())
}

/// Whether an SDP line is one of the media stack's (module doc).
fn sdp_counter(line: &[u8]) -> bool {
    match line[0] {
        b'v' | b'c' | b't' | b'r' | b'z' | b'm' | b'b' | b'k' => true,
        b'a' => {
            let attr = &line[2..];
            let name = &attr[..attr.iter().position(|b| *b == b':').unwrap_or(attr.len())];
            MEDIA_ATTRIBUTES.contains(&String::from_utf8_lossy(name).to_ascii_lowercase().as_str())
        }
        _ => false,
    }
}

/// Whether the run at `start` is the value of a counter parameter
/// (`;expires=3600`).
fn is_counter_param(input: &[u8], start: usize) -> bool {
    if start == 0 || input[start - 1] != b'=' {
        return false;
    }
    let name_start = input[..start - 1]
        .iter()
        .rposition(|b| !(b.is_ascii_alphanumeric() || *b == b'-'))
        .map_or(0, |p| p + 1);
    let name = String::from_utf8_lossy(&input[name_start..start - 1]).to_ascii_lowercase();
    name_start > 0 && input[name_start - 1] == b';' && COUNTER_PARAMS.contains(&name.as_str())
}

/// Whether `input[i]` is one of the two hex digits of a percent escape
/// (`%40`, RFC 3261 §19.1.2): an escaped byte's spelling, never a digit of a
/// run.
pub(crate) fn in_escape(input: &[u8], i: usize) -> bool {
    let hex = |j: usize| input.get(j).is_some_and(u8::is_ascii_hexdigit);
    (i >= 1 && input[i - 1] == b'%' && hex(i) && hex(i + 1))
        || (i >= 2 && input[i - 2] == b'%' && hex(i - 1) && hex(i))
}

/// The maximal digit runs of `input` outside percent escapes, as spans.
pub(crate) fn digit_runs(input: &[u8]) -> Vec<std::ops::Range<usize>> {
    let digit = |i: usize| input[i].is_ascii_digit() && !in_escape(input, i);
    let mut out = Vec::new();
    let mut i = 0;
    while i < input.len() {
        if !digit(i) {
            i += 1;
            continue;
        }
        let start = i;
        while i < input.len() && digit(i) {
            i += 1;
        }
        out.push(start..i);
    }
    out
}

/// Whether the run is the port of a hostport (module doc).
fn is_port(input: &[u8], start: usize, end: usize) -> bool {
    if start < 2 || input[start - 1] != b':' {
        return false;
    }
    let ends = input.get(end).is_none_or(|b| {
        b.is_ascii_whitespace()
            || matches!(b, b';' | b'>' | b'/' | b'?' | b',' | b'"' | b']' | b'|')
    });
    if !ends {
        return false;
    }
    if input[start - 2] == b']' {
        return true;
    }
    let host_byte = |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_');
    let before = input[..start - 1].iter().rposition(|b| !host_byte(b));
    let host = &input[before.map_or(0, |i| i + 1)..start - 1];
    let lower = String::from_utf8_lossy(host).to_ascii_lowercase();
    if host.is_empty() || SCHEMES.contains(&lower.as_str()) || !opens_host(input, before) {
        return false;
    }
    host.iter().any(u8::is_ascii_alphabetic) || crate::fake::address::ipv4_octets(&lower).is_some()
}

/// Whether a host starts behind `before` (the byte ahead of it, `None` at
/// the text's start): a socket opening the text, a userinfo's `@`, a SIP or
/// URL scheme's `:`, an authority's `//`, or the space behind a Via
/// sent-protocol (`SIP/2.0/UDP`) or a Warning code.
fn opens_host(input: &[u8], before: Option<usize>) -> bool {
    let Some(at) = before else { return true };
    let token_before = |end: usize| {
        let start = input[..end]
            .iter()
            .rposition(|b| b.is_ascii_whitespace() || matches!(b, b'<' | b',' | b'"' | b';'))
            .map_or(0, |p| p + 1);
        String::from_utf8_lossy(&input[start..end]).to_ascii_lowercase()
    };
    match input[at] {
        b'@' => true,
        b':' => SCHEMES.contains(&token_before(at).as_str()),
        b'/' => at >= 1 && input[at - 1] == b'/',
        b' ' | b'\t' => {
            let token = token_before(at);
            token.starts_with("sip/2.0/")
                || (token.len() == 3
                    && token.bytes().all(|b| b.is_ascii_digit())
                    && header_name(input, at).as_deref() == Some("warning"))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests;
