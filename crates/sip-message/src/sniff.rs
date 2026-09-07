//! Lenient raw-datagram scanners: extract one field from an unparsed SIP
//! datagram without a full parse. For hot paths (demux, dedup, metrics
//! labelling) that must stay cheap and tolerate malformed input by returning
//! `None`/empty rather than erroring.
//!
//! This module is the ONLY sanctioned home for raw SIP header extraction —
//! never re-implement these scanners in another crate. If a scanner you need
//! is missing, add it here. For anything richer than single-field extraction,
//! use the real parser ([`crate::parser`]). Strict, allocation-free
//! pre-parse *classifiers* (overload brake, dispatcher fast-path) live in
//! [`crate::preparse`] and its sibling modules.

use std::borrow::Cow;

use crate::parser::custom::scanner::is_token_char;
use crate::trace_sample::{TraceSample, TRACE_SAMPLE_HEADER};

fn as_str(raw: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(raw)
}

/// The (trimmed) request/status line, empty if the datagram has none.
pub fn first_line(raw: &[u8]) -> String {
    as_str(raw).lines().next().unwrap_or("").trim().to_string()
}

/// Value of header `name` (case-insensitive), scanning the header block only.
pub fn header_value(raw: &[u8], name: &str) -> Option<String> {
    let s = as_str(raw);
    for line in s.lines() {
        if line.is_empty() {
            break; // end of headers
        }
        if let Some((h, v)) = line.split_once(':') {
            if h.trim().eq_ignore_ascii_case(name) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// EVERY value of header `name`, one entry per header ROW in wire order — the
/// repeated-header read (`Route`, `Record-Route`, `Contact`) for a comparison
/// that weighs what one message carried against another. A comma fold inside
/// one row stays ONE value: the row is what the sender wrote, and a rule
/// comparing two messages compares the rows they carried.
///
/// Matches the compact spelling too (`m` for `Contact`), like every other
/// reader here: a row is the sender's, whichever of the two names it used.
pub fn header_values(raw: &[u8], name: &str) -> Vec<String> {
    header_rows_compact(raw, name)
}

/// The Call-ID (full or compact `i` form).
pub fn call_id(raw: &[u8]) -> Option<String> {
    header_value(raw, "call-id").or_else(|| header_value(raw, "i"))
}

/// The `tag` parameter of the To header (full or compact `t` form), or the
/// empty string when absent (e.g. a tagless 100 Trying).
pub fn to_tag(raw: &[u8]) -> String {
    let Some(v) = header_value(raw, "to").or_else(|| header_value(raw, "t")) else {
        return String::new();
    };
    let lower = v.to_ascii_lowercase();
    let Some(pos) = lower.find("tag=") else { return String::new() };
    let rest = &v[pos + "tag=".len()..];
    let end = rest.find([';', ',', ' ', '\t', '>']).unwrap_or(rest.len());
    rest[..end].trim().to_string()
}

/// Whether `raw` is a SIP response (status line) vs a request.
pub fn is_response(raw: &[u8]) -> bool {
    first_line(raw).starts_with("SIP/2.0")
}

/// The response status code (`200` from `SIP/2.0 200 OK`), or `None` for a request.
pub fn resp_status(raw: &[u8]) -> Option<u16> {
    let line = first_line(raw);
    if !line.starts_with("SIP/2.0") {
        return None;
    }
    line.split_whitespace().nth(1).and_then(|s| s.parse().ok())
}

/// The request method (`INVITE` from the request line), or `None` for a response.
pub fn req_method(raw: &[u8]) -> Option<String> {
    let line = first_line(raw);
    if line.starts_with("SIP/2.0") {
        return None;
    }
    line.split_whitespace().next().map(str::to_string)
}

/// The Request-URI (the second token of the request line), or `None` for a
/// response. The token is returned exactly as it appears — a demux tier that
/// keys on the URI reads it with the URI value type.
pub fn request_uri(raw: &[u8]) -> Option<String> {
    let line = first_line(raw);
    if line.starts_with("SIP/2.0") {
        return None;
    }
    line.split_whitespace().nth(1).map(str::to_string)
}

/// The CSeq sequence number (the `<num>` of `CSeq: <num> <METHOD>`), or `None`
/// if absent/unparseable. Works for requests and responses.
pub fn cseq_number(raw: &[u8]) -> Option<u32> {
    cseq_line(raw)?.split_whitespace().next()?.parse().ok()
}

/// The CSeq line rendered for a log/sample (`CSeq: <num> <METHOD>`), empty if
/// absent.
pub fn cseq_value(raw: &[u8]) -> String {
    match cseq_line(raw) {
        Some(v) => format!("CSeq: {v}"),
        None => String::new(),
    }
}

/// The CSeq method mapped to a BOUNDED static label — safe as a low-cardinality
/// metrics label AND usable for method comparison (every RFC 3261/3262/3515
/// method maps to itself). `"none"` when absent, `"other"` for an unknown
/// method.
pub fn cseq_method_label(raw: &[u8]) -> &'static str {
    let Some(l) = cseq_line(raw) else { return "none" };
    let m = l.split_whitespace().nth(1).unwrap_or("");
    match m.to_ascii_uppercase().as_str() {
        "INVITE" => "INVITE",
        "ACK" => "ACK",
        "BYE" => "BYE",
        "CANCEL" => "CANCEL",
        "OPTIONS" => "OPTIONS",
        "REFER" => "REFER",
        "NOTIFY" => "NOTIFY",
        "PRACK" => "PRACK",
        "UPDATE" => "UPDATE",
        "INFO" => "INFO",
        "SUBSCRIBE" => "SUBSCRIBE",
        "MESSAGE" => "MESSAGE",
        "" => "none",
        _ => "other",
    }
}

/// The trimmed CSeq header value (`<num> <METHOD>`), or `None`.
fn cseq_line(raw: &[u8]) -> Option<String> {
    let s = as_str(raw);
    for line in s.lines() {
        let l = line.trim();
        if l.len() >= 5 && l[..5].eq_ignore_ascii_case("cseq:") {
            return Some(l[5..].trim().to_string());
        }
    }
    None
}

/// Whether a `Require` header lists the `100rel` option-tag (comma-folded,
/// case-insensitive) — the reliable-provisional marker (RFC 3262 §3). `Require`
/// has no compact form.
pub fn require_has_100rel(raw: &[u8]) -> bool {
    header_value(raw, "require")
        .is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("100rel")))
}

/// The `RSeq` value of a reliable provisional (RFC 3262 §3), or `None`.
pub fn rseq_of(raw: &[u8]) -> Option<u64> {
    header_value(raw, "rseq")?.trim().parse().ok()
}

/// The RAck response-num (its FIRST token = the acknowledged 1xx's RSeq,
/// RFC 3262 §7.2) of a PRACK, or `None`.
pub fn rack_rseq(raw: &[u8]) -> Option<u64> {
    header_value(raw, "rack")?.split_whitespace().next()?.parse().ok()
}

/// The RAck CSeq-num (its SECOND token = the sequence number of the INVITE the
/// acknowledged provisional answered, RFC 3262 §7.2) of a PRACK, or `None`.
///
/// `RAck` names its target by three fields, and the response-num alone does
/// not: two INVITEs on one dialog number their `RSeq` spaces independently, so
/// a reader joining a PRACK to the provisional it acknowledges needs this one
/// too.
pub fn rack_cseq(raw: &[u8]) -> Option<u32> {
    header_value(raw, "rack")?.split_whitespace().nth(1)?.parse().ok()
}

/// Selective intake-shed classifier: whether a raw datagram is a NEW-DIALOG,
/// NON-EMERGENCY INVITE — the only class a depth-watermarked pre-ingress hook
/// may drop under overload. Called per-datagram at the socket pump, zero
/// allocation; `false` = admit.
///
/// `true` requires ALL of: the canonical `INVITE ` request line
/// ([`crate::preparse::is_invite_request_buffer`]), a To header (full or
/// compact `t` form, like [`to_tag`]) WITHOUT a `tag=` parameter (new
/// dialog), and no emergency Resource-Priority — any `Resource-Priority`
/// header (case-insensitive name, comma-separated r-values) carrying an
/// emergency r-value classifies exactly as [`crate::emergency`]'s
/// parsed-side check. Everything else — in-dialog requests, responses,
/// other methods, emergency INVITEs, truncated/garbage datagrams — is
/// admitted (garbage is cheap to admit; the parse layer discards it).
pub fn is_sheddable_new_invite(raw: &[u8]) -> bool {
    if !crate::preparse::is_invite_request_buffer(raw) {
        return false;
    }
    let mut lines = raw.split(|&b| b == b'\n');
    lines.next(); // request line
    let mut tagless_to_seen = false;
    for line in lines {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            break; // end of headers
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else { continue };
        let name = line[..colon].trim_ascii();
        let value = &line[colon + 1..];
        if name.eq_ignore_ascii_case(b"to") || name.eq_ignore_ascii_case(b"t") {
            if contains_ignore_ascii_case(value, b"tag=") {
                return false; // in-dialog
            }
            tagless_to_seen = true;
        } else if name.eq_ignore_ascii_case(b"resource-priority")
            && value.split(|&b| b == b',').any(|rv| {
                crate::emergency::EMERGENCY_RPH_TOKENS
                    .iter()
                    .any(|tok| rv.trim_ascii().eq_ignore_ascii_case(tok.as_bytes()))
            })
        {
            return false; // emergency
        }
    }
    tagless_to_seen
}

/// ASCII-case-insensitive substring search, allocation-free. `needle` must be
/// non-empty.
fn contains_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
}

/// The longest `X-Trace-Sample` value [`trace_sample_rate`] reads. A rate is a
/// short float; anything longer states something this reader will not act on.
pub const TRACE_SAMPLE_VALUE_CAP: usize = 32;

/// The `X-Trace-Sample` sampling rate an INVITE **datagram** asks for, read off
/// the raw bytes (ADR-0026) — the proxy's entry path, which samples before it
/// has a call. Non-INVITE datagrams read [`TraceSample::Absent`]: only an
/// initial INVITE opens a trace, so no other datagram is scanned.
///
/// Same value grammar as the full-parse reader
/// ([`crate::trace_sample::trace_sample`]): a float in `0..=1` reads, the FIRST
/// instance wins, and a value that does not read is [`TraceSample::Malformed`]
/// — kept apart from absent so a rig that mistyped its rate is counted, not
/// silently ignored. A value longer than [`TRACE_SAMPLE_VALUE_CAP`] is
/// malformed by length alone. Allocation-free.
pub fn trace_sample_rate(raw: &[u8]) -> TraceSample {
    if !crate::preparse::is_invite_request_buffer(raw) {
        return TraceSample::Absent;
    }
    let mut lines = raw.split(|&b| b == b'\n');
    lines.next(); // request line
    for line in lines {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            break; // end of headers
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else { continue };
        if !line[..colon].trim_ascii().eq_ignore_ascii_case(TRACE_SAMPLE_HEADER.as_bytes()) {
            continue;
        }
        let value = line[colon + 1..].trim_ascii();
        if value.len() > TRACE_SAMPLE_VALUE_CAP {
            return TraceSample::Malformed;
        }
        return match std::str::from_utf8(value).ok().and_then(|v| v.parse::<f64>().ok()) {
            Some(rate) if rate.is_finite() && (0.0..=1.0).contains(&rate) => TraceSample::Rate(rate),
            _ => TraceSample::Malformed,
        };
    }
    TraceSample::Absent
}

/// The `branch` parameter of the TOP-most Via header (RFC 3261 §17 transaction
/// key), or `None` if absent. Only the first Via matters — on a request we sent
/// it is OUR Via, echoed by the UAS onto the matching response.
pub fn via_branch(raw: &[u8]) -> Option<String> {
    for line in as_str(raw).lines() {
        if line.is_empty() {
            break; // end of headers
        }
        let Some((h, v)) = line.split_once(':') else { continue };
        let h = h.trim();
        if h.eq_ignore_ascii_case("via") || h.eq_ignore_ascii_case("v") {
            let pos = v.find("branch=")?;
            let rest = &v[pos + "branch=".len()..];
            let end = rest.find([';', ',', ' ', '\t']).unwrap_or(rest.len());
            let b = rest[..end].trim();
            return (!b.is_empty()).then(|| b.to_string());
        }
    }
    None
}

/// Every row of `name`, resolving the RFC 3261 §7.3.3 compact spelling: a
/// scanner asked for `Supported` also reads the `k` rows the wire may carry.
fn header_rows_compact(raw: &[u8], name: &str) -> Vec<String> {
    let s = as_str(raw);
    let mut out = Vec::new();
    for line in s.lines() {
        if line.is_empty() {
            break; // end of headers
        }
        let Some((h, v)) = line.split_once(':') else { continue };
        if crate::parser::custom::compact_forms::expanded_name(h.trim())
            .eq_ignore_ascii_case(name)
        {
            out.push(v.trim().to_string());
        }
    }
    out
}

/// Whether `name` appears at all, compact spelling included — the read for a
/// rule that only asks whether a capability header was advertised, never what
/// it listed. A header written with an empty value is PRESENT.
pub fn has_header(raw: &[u8], name: &str) -> bool {
    !header_rows_compact(raw, name).is_empty()
}

/// The line of `raw` that spans byte `offset` — the header row (or, past the
/// blank line, the body line) a byte-level divergence between two copies of one
/// message sits in — without its line ending. `None` where `offset` is past the
/// end of `raw`, which is where a copy that is a strict prefix of another stops.
pub fn line_at(raw: &[u8], offset: usize) -> Option<String> {
    if offset >= raw.len() {
        return None;
    }
    let start = raw[..offset].iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
    let end = raw[offset..].iter().position(|&b| b == b'\n').map_or(raw.len(), |p| offset + p);
    Some(as_str(&raw[start..end]).trim_end_matches('\r').to_string())
}

/// The body length a message declares in `Content-Length` (compact `l`), or
/// `None` where the header is absent or no reader accepts its value.
pub fn content_length(raw: &[u8]) -> Option<u64> {
    header_rows_compact(raw, "Content-Length").first()?.trim().parse().ok()
}

/// Whether a message carries a body, read off its HEADER block alone — the read
/// for a rule that asks whether an offer or answer rode a message, from a
/// vantage that recorded no body bytes.
///
/// `Content-Length` states it (RFC 3261 §20.14: a message with no body sets the
/// field to zero); where the field is absent or unreadable, a `Content-Type` is
/// the message naming a body it carries, and a head with neither declares none.
pub fn has_body(raw: &[u8]) -> bool {
    match content_length(raw) {
        Some(len) => len > 0,
        None => has_header(raw, "Content-Type"),
    }
}

/// Whether the message's `Content-Type` (compact `c`) NAMES `media_type` — the
/// read for a rule that judges a body only under the format it was declared as.
/// Comparison is on the media type alone: the value's parameters
/// (`;charset=…`, `;boundary=…`) neither hide it nor make another one match.
pub fn content_type_is(raw: &[u8], media_type: &str) -> bool {
    use crate::header::{HeaderValue, MediaType};
    header_rows_compact(raw, "Content-Type").iter().any(|row| {
        MediaType::parse(&crate::sip_str::SipStr::owned(row)).is_ok_and(|ct| ct.is(media_type))
    })
}

/// The body bytes of a raw datagram: everything after the empty line that ends
/// the header block (RFC 3261 §7). `None` where no such line is present — the
/// head is unterminated and the datagram states nothing about a body.
///
/// The empty line is CRLFCRLF on the wire; a bare LFLF is accepted the way the
/// lenient parser accepts it, so a capture normalised to LF still reads. The
/// declared `Content-Length` is NOT applied: this returns what the datagram
/// carried, and a consumer that needs the declared length reads
/// [`content_length`].
pub fn body(raw: &[u8]) -> Option<&[u8]> {
    let crlf = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4);
    let lf = raw.windows(2).position(|w| w == b"\n\n").map(|i| i + 2);
    let cut = match (crlf, lf) {
        (Some(a), Some(b)) => a.min(b),
        (a, b) => a.or(b)?,
    };
    Some(&raw[cut..])
}

/// The option tags a set-like header lists (`Require`, `Supported`,
/// `Unsupported`, `Allow`, …), in wire order — the read for a rule that weighs
/// what one message required against what another advertised.
///
/// One SET, exactly as [`crate::header::TokenListHeader`] reads it: repeated
/// rows and the separator folds inside a row union together, a tag already in
/// the set is not repeated (option tags are case-insensitive, RFC 3261 §7.3.1),
/// and a value that is not a `token` — an embedded space, a stray CRLF — is
/// dropped rather than minting a tag nobody wrote. An absent header is the
/// empty set.
pub fn option_tags(raw: &[u8], name: &str) -> Vec<String> {
    let sep = crate::header::HeaderName::item_separator_of(name);
    let mut out: Vec<String> = Vec::new();
    for row in header_rows_compact(raw, name) {
        for tag in sep.split(&row) {
            let ok = !tag.is_empty() && tag.bytes().all(is_token_char);
            if ok && !out.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
                out.push(tag.to_string());
            }
        }
    }
    out
}

/// The URI of an address-valued header (`From`, `To`, `Contact`, …) as the
/// value reader sees it, or `None` when the header is absent or no reader
/// accepts it. Header parameters (`;tag=`) stay off the URI, which is what
/// separates a dialog's peer URI from its tag.
pub fn name_addr_uri(raw: &[u8], name: &str) -> Option<String> {
    use crate::header::{From as FromHeader, HeaderValue};
    let row = header_rows_compact(raw, name).into_iter().next()?;
    // Read under the most permissive name-addr kind — every address header
    // shares one grammar, and only the header's OWN parameters differ, which
    // this read discards.
    let addr = FromHeader::parse(&crate::sip_str::SipStr::owned(&row)).ok()?;
    Some(addr.uri().to_string())
}

/// What one URI states about where a message goes: the canonical text, the
/// host and effective port RFC 3263 §4 resolves it to, and whether it is a
/// loose route (`;lr`, RFC 3261 §19.1.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UriFacts {
    /// The URI rendered from its parts — the spelling two URIs compare on, so
    /// whitespace and the original layout never decide equality.
    pub uri: String,
    pub host: String,
    /// The port the URI names, defaulted when it names none.
    pub port: u16,
    /// The URI carries `;lr`.
    pub loose: bool,
}

impl UriFacts {
    fn of(uri: &crate::header::Uri) -> Self {
        let (host, port) = uri.host_port();
        UriFacts {
            uri: uri.to_string(),
            host: host.to_string(),
            port,
            loose: uri.is_loose_route(),
        }
    }
}

/// The `Route` / `Record-Route` hops `raw` carries, in wire order and with
/// comma folds split (RFC 3261 §7.3.1 lets one row carry several hops, so a
/// count taken per row would miscount a folded set).
///
/// `Some(vec![])` is "the header is absent"; `None` is "a row is there that no
/// reader accepts" — a routing rule states nothing about a hop nobody can
/// resolve, and the grammar rules own the malformed row.
pub fn route_uris(raw: &[u8], name: &str) -> Option<Vec<UriFacts>> {
    use crate::header::{HeaderValue, RouteEntry};
    let mut out = Vec::new();
    for row in header_rows_compact(raw, name) {
        let hops = RouteEntry::parse_line(&crate::sip_str::SipStr::owned(&row)).ok()?;
        out.extend(hops.iter().map(|h| UriFacts::of(h.uri())));
    }
    Some(out)
}

/// One `Route` / `Record-Route` row as the datagram carries it: where its line
/// sits in the raw bytes, and the hops it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteRow {
    /// The row's bytes in the datagram, its line ending included, so leaving
    /// the span out of the datagram leaves the neighbouring rows adjacent.
    pub span: std::ops::Range<usize>,
    /// The hops the row names, comma folds split; `None` where no reader
    /// accepts the row.
    pub hops: Option<Vec<UriFacts>>,
}

/// Every `name` row (`Route` / `Record-Route`) of the header block, in wire
/// order, each with its byte span — the read for a comparison that leaves a
/// row out of two copies of one datagram and still locates a disagreement in
/// the ORIGINAL bytes. Compact spellings resolve as in every other row read;
/// the scan stops at the blank line that ends the head.
pub fn route_rows(raw: &[u8], name: &str) -> Vec<RouteRow> {
    use crate::header::{HeaderValue, RouteEntry};
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < raw.len() {
        let line_end = raw[pos..].iter().position(|&b| b == b'\n').map_or(raw.len(), |p| pos + p);
        let next = (line_end + 1).min(raw.len());
        let line = as_str(&raw[pos..line_end]);
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break; // end of headers
        }
        if let Some((h, v)) = line.split_once(':') {
            if crate::parser::custom::compact_forms::expanded_name(h.trim()).eq_ignore_ascii_case(name) {
                let hops = RouteEntry::parse_line(&crate::sip_str::SipStr::owned(v.trim()))
                    .ok()
                    .map(|hops| hops.iter().map(|h| UriFacts::of(h.uri())).collect());
                out.push(RouteRow { span: pos..next, hops });
            }
        }
        pos = next;
    }
    out
}

/// The Request-URI's routing facts, or `None` for a response or a request line
/// whose URI no reader accepts.
pub fn request_uri_facts(raw: &[u8]) -> Option<UriFacts> {
    let text = request_uri(raw)?;
    let uri = crate::header::Uri::parse(&crate::sip_str::SipStr::owned(&text)).ok()?;
    Some(UriFacts::of(&uri))
}

/// What the TOP-most Via says about `rport` (RFC 3581): the four states a
/// server's echo can be in, kept apart because the obligation differs — a bare
/// request asks, and a response owes the observed port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViaRport {
    /// No `rport` parameter at all.
    Absent,
    /// `;rport` bare — the sender asking to be told its source port.
    Requested,
    /// `;rport=<port>` — the port the next hop observed.
    Observed(u16),
    /// `rport` is there carrying a value no reader accepts.
    Unreadable,
}

/// The `rport` parameter of the TOP-most Via (RFC 3581 §4). Only the first Via
/// matters: it is the hop whose response routing `rport` fixes.
pub fn via_rport(raw: &[u8]) -> ViaRport {
    use crate::header::{HeaderValue, ParamValue, Rport, Via};
    for line in as_str(raw).lines() {
        if line.is_empty() {
            break; // end of headers
        }
        let Some((h, v)) = line.split_once(':') else { continue };
        let h = h.trim();
        if !(h.eq_ignore_ascii_case("via") || h.eq_ignore_ascii_case("v")) {
            continue;
        }
        let Ok(vias) = Via::parse_line(&crate::sip_str::SipStr::owned(v.trim()))
        else {
            return ViaRport::Unreadable;
        };
        let Some(top) = vias.first() else { return ViaRport::Unreadable };
        return match (top.param("rport"), top.rport()) {
            (None, _) => ViaRport::Absent,
            (Some(ParamValue::Flag), _) => ViaRport::Requested,
            (Some(_), Some(Rport::Observed(port))) => ViaRport::Observed(port),
            (Some(_), _) => ViaRport::Unreadable,
        };
    }
    ViaRport::Absent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_at_names_the_row_spanning_an_offset() {
        let raw = b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP h;branch=z9hG4bK1\r\n\r\nv=0\r\no=- 1 1 IN IP4 h\r\n";
        assert_eq!(line_at(raw, 0).as_deref(), Some("SIP/2.0 200 OK"));
        let via_at = raw.iter().position(|&b| b == b'V').unwrap();
        assert_eq!(
            line_at(raw, via_at + 10).as_deref(),
            Some("Via: SIP/2.0/UDP h;branch=z9hG4bK1"),
            "an offset inside a row names the whole row"
        );
        let body_at = raw.len() - 3;
        assert_eq!(line_at(raw, body_at).as_deref(), Some("o=- 1 1 IN IP4 h"), "body lines read the same way");
        assert_eq!(line_at(raw, raw.len()), None, "past the end names nothing");
    }

    #[test]
    fn to_tag_extracts_the_to_parameter() {
        assert_eq!(
            to_tag(b"SIP/2.0 180 X\r\nTo: <sip:b@h>;tag=abc\r\n\r\n"),
            "abc"
        );
        assert_eq!(
            to_tag(b"SIP/2.0 100 Trying\r\nTo: <sip:b@h>\r\n\r\n"),
            "",
            "a tagless To yields empty"
        );
        assert_eq!(
            to_tag(b"SIP/2.0 200 OK\r\nt: <sip:b@h>;tag=Z9\r\n\r\n"),
            "Z9",
            "the compact To form is parsed"
        );
    }

    #[test]
    fn cseq_scanners_share_one_line_walk() {
        let raw = b"BYE sip:x SIP/2.0\r\nCSeq:  7   BYE\r\n\r\n";
        assert_eq!(cseq_number(raw), Some(7));
        assert_eq!(cseq_value(raw), "CSeq: 7   BYE");
        assert_eq!(cseq_method_label(raw), "BYE");
        assert_eq!(cseq_method_label(b"OPTIONS sip:x SIP/2.0\r\n\r\n"), "none");
        assert_eq!(
            cseq_method_label(b"X sip:x SIP/2.0\r\nCSeq: 1 WEIRD\r\n\r\n"),
            "other"
        );
    }

    #[test]
    fn option_tags_union_rows_and_folds_into_one_set() {
        let raw = b"INVITE sip:b@h SIP/2.0\r\n\
                    Supported: 100rel, timer\r\n\
                    k: 100REL,replaces\r\n\r\n";
        assert_eq!(
            option_tags(raw, "Supported"),
            ["100rel", "timer", "replaces"],
            "rows and comma folds are one set, the compact spelling included, \
             and a repeat of a tag already in it adds nothing"
        );
        assert_eq!(option_tags(raw, "Require"), Vec::<String>::new(), "absent is empty");
    }

    #[test]
    fn option_tags_drops_a_value_that_is_not_a_token() {
        let raw = b"INVITE sip:b@h SIP/2.0\r\nRequire: two words, timer,\r\n\r\n";
        assert_eq!(option_tags(raw, "Require"), ["timer"]);
    }

    /// RFC 3323 §4.2 gives `Privacy` a `;`-separated list; the reader follows
    /// the header's own grammar rather than assuming a comma.
    #[test]
    fn option_tags_follow_the_headers_own_separator() {
        let raw = b"INVITE sip:b@h SIP/2.0\r\nPrivacy: id;user\r\n\r\n";
        assert_eq!(option_tags(raw, "Privacy"), ["id", "user"]);
    }

    #[test]
    fn request_uri_reads_the_second_request_line_token() {
        assert_eq!(
            request_uri(b"INVITE sip:bob@10.0.0.1:5070 SIP/2.0\r\n\r\n").as_deref(),
            Some("sip:bob@10.0.0.1:5070")
        );
        assert_eq!(
            request_uri(b"SIP/2.0 200 OK\r\n\r\n"),
            None,
            "a response has no Request-URI"
        );
    }

    /// Assemble a request with the given request-line method token and extra
    /// header lines (fixture only — the classifier under test never allocates).
    fn req(method: &str, headers: &str) -> Vec<u8> {
        format!(
            "{method} sip:bob@10.0.0.2:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-shed\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
{headers}Call-ID: shed@10.0.0.1\r\n\
CSeq: 1 {method}\r\n\
Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn new_invite_is_sheddable() {
        assert!(is_sheddable_new_invite(&req("INVITE", "To: <sip:bob@example.com>\r\n")));
    }

    #[test]
    fn to_tag_marks_in_dialog_and_admits() {
        assert!(!is_sheddable_new_invite(&req(
            "INVITE",
            "To: <sip:bob@example.com>;tag=b2\r\n"
        )));
    }

    #[test]
    fn to_header_name_casing_and_compact_form_are_recognised() {
        for tagless in ["TO: <sip:bob@h>\r\n", "to: <sip:bob@h>\r\n", "t: <sip:bob@h>\r\n"] {
            assert!(is_sheddable_new_invite(&req("INVITE", tagless)), "{tagless:?}");
        }
        for tagged in ["TO: <sip:bob@h>;TAG=B2\r\n", "t: <sip:bob@h>;tag=b2\r\n"] {
            assert!(!is_sheddable_new_invite(&req("INVITE", tagged)), "{tagged:?}");
        }
    }

    #[test]
    fn non_invite_methods_are_admitted() {
        for m in ["ACK", "BYE", "CANCEL", "OPTIONS", "REGISTER"] {
            assert!(
                !is_sheddable_new_invite(&req(m, "To: <sip:bob@example.com>\r\n")),
                "{m} must be admitted"
            );
        }
    }

    #[test]
    fn responses_are_admitted() {
        assert!(!is_sheddable_new_invite(
            b"SIP/2.0 200 OK\r\nTo: <sip:bob@h>\r\nCSeq: 1 INVITE\r\n\r\n"
        ));
        assert!(!is_sheddable_new_invite(b"SIP/2.0 180 Ringing\r\nTo: <sip:bob@h>\r\n\r\n"));
    }

    #[test]
    fn emergency_invites_are_admitted() {
        // Each canonical r-value, mixed case, and a comma-separated list.
        for rph in ["esnet.0", "wps.0", "q735.0", "ESNET.0", "Wps.0", "dsn.flash, q735.0"] {
            let raw = req(
                "INVITE",
                &format!("To: <sip:bob@h>\r\nResource-Priority: {rph}\r\n"),
            );
            assert!(!is_sheddable_new_invite(&raw), "{rph:?} must be admitted");
        }
        // Case-insensitive header name.
        assert!(!is_sheddable_new_invite(&req(
            "INVITE",
            "To: <sip:bob@h>\r\nRESOURCE-PRIORITY: esnet.0\r\n"
        )));
        // Any of multiple Resource-Priority headers flags.
        assert!(!is_sheddable_new_invite(&req(
            "INVITE",
            "To: <sip:bob@h>\r\nResource-Priority: dsn.flash\r\nResource-Priority: wps.0\r\n"
        )));
    }

    #[test]
    fn non_emergency_resource_priority_stays_sheddable() {
        // r-values compare whole (comma-split, trimmed) — mirrors
        // `crate::emergency`: `dsn.flash` and the embedded `esnet.01` are not
        // emergency, so the new INVITE remains sheddable.
        for rph in ["dsn.flash", "esnet.01"] {
            let raw = req(
                "INVITE",
                &format!("To: <sip:bob@h>\r\nResource-Priority: {rph}\r\n"),
            );
            assert!(is_sheddable_new_invite(&raw), "{rph:?} is not emergency");
        }
    }

    #[test]
    fn truncated_and_garbage_datagrams_are_admitted() {
        for raw in [
            &b""[..],
            &b"INVITE"[..],
            &b"INVITE sip:bob@h SIP/2.0\r\nVia: SIP/2.0/UDP x\r\n\r\n"[..], // no To at all
            &b"invite sip:bob@h SIP/2.0\r\nTo: <sip:bob@h>\r\n\r\n"[..],    // method case-sensitive
            &b"\x00\x01\x02 garbage \xff\xfe"[..],
        ] {
            assert!(!is_sheddable_new_invite(raw), "{raw:?} must be admitted");
        }
    }

    #[test]
    fn trace_sample_rate_reads_a_float_in_range_off_the_raw_invite() {
        for (wire, expected) in [("1", 1.0), ("0", 0.0), ("0.25", 0.25), ("  0.5 ", 0.5)] {
            let raw = req("INVITE", &format!("X-Trace-Sample: {wire}\r\n"));
            assert_eq!(trace_sample_rate(&raw), TraceSample::Rate(expected), "{wire}");
        }
    }

    #[test]
    fn trace_sample_rate_matches_the_full_parse_readers_grammar() {
        // Absent and malformed stay apart, out-of-range is refused, the first
        // instance wins, and the header name is case-insensitive — the same
        // contract `trace_sample::trace_sample` states over a parsed request.
        assert_eq!(trace_sample_rate(&req("INVITE", "")), TraceSample::Absent);
        for bad in ["yes", "1.5", "-0.1", "NaN", "inf", "0.5,0.9", ""] {
            let raw = req("INVITE", &format!("X-Trace-Sample: {bad}\r\n"));
            assert_eq!(trace_sample_rate(&raw), TraceSample::Malformed, "{bad:?}");
        }
        let repeated = req("INVITE", "X-Trace-Sample: 0.1\r\nX-Trace-Sample: 1\r\n");
        assert_eq!(trace_sample_rate(&repeated), TraceSample::Rate(0.1));
        let lower = req("INVITE", "x-trace-sample: 0.75\r\n");
        assert_eq!(trace_sample_rate(&lower), TraceSample::Rate(0.75));
    }

    #[test]
    fn an_over_long_value_is_malformed_by_length_alone() {
        let raw = req("INVITE", &format!("X-Trace-Sample: 0.{}\r\n", "1".repeat(64)));
        assert_eq!(trace_sample_rate(&raw), TraceSample::Malformed);
    }

    #[test]
    fn only_an_invite_datagram_is_scanned() {
        for m in ["ACK", "BYE", "CANCEL", "OPTIONS", "REGISTER"] {
            let raw = req(m, "X-Trace-Sample: 1\r\n");
            assert_eq!(trace_sample_rate(&raw), TraceSample::Absent, "{m} opens no trace");
        }
        assert_eq!(
            trace_sample_rate(b"SIP/2.0 200 OK\r\nX-Trace-Sample: 1\r\n\r\n"),
            TraceSample::Absent,
        );
    }

    #[test]
    fn a_body_carrying_the_header_name_is_not_scanned() {
        let mut raw = req("INVITE", "");
        raw.extend_from_slice(b"X-Trace-Sample: 1\r\n");
        assert_eq!(trace_sample_rate(&raw), TraceSample::Absent, "the scan stops at the body");
    }

    #[test]
    fn header_values_reads_every_row_in_wire_order() {
        let raw = b"CANCEL sip:x SIP/2.0\r\n\
                    Route: <sip:p1@h;lr>\r\n\
                    Route: <sip:p2@h;lr>, <sip:p3@h;lr>\r\n\
                    Call-ID: c1\r\n\r\nRoute: <sip:body@h>\r\n";
        assert_eq!(
            header_values(raw, "route"),
            vec!["<sip:p1@h;lr>".to_string(), "<sip:p2@h;lr>, <sip:p3@h;lr>".to_string()],
            "one entry per row, comma folds kept whole, the scan stops at the body",
        );
        assert!(header_values(raw, "record-route").is_empty(), "an absent header reads empty");
    }

    #[test]
    fn header_values_reads_the_compact_spelling_too() {
        // `m` is Contact (RFC 3261 §20): a REGISTER's binding is the sender's
        // whichever name it wrote, so both rows are read, in wire order.
        let raw = b"REGISTER sip:h SIP/2.0\r\n\
                    Contact: <sip:a@1>\r\n\
                    m: <sip:a@2>\r\n\
                    Call-ID: c1\r\n\r\n";
        assert_eq!(
            header_values(raw, "contact"),
            vec!["<sip:a@1>".to_string(), "<sip:a@2>".to_string()],
        );
    }

    #[test]
    fn via_branch_takes_topmost_via_only() {
        let raw = b"INVITE sip:x SIP/2.0\r\nVia: SIP/2.0/UDP a;branch=z9-top\r\nVia: SIP/2.0/UDP b;branch=z9-bot\r\n\r\n";
        assert_eq!(via_branch(raw).as_deref(), Some("z9-top"));
        assert_eq!(via_branch(b"ACK sip:x SIP/2.0\r\n\r\n"), None);
    }

    #[test]
    fn has_header_reads_the_compact_spelling_and_an_empty_value() {
        let raw = b"INVITE sip:x SIP/2.0\r\nAllow: INVITE, ACK\r\nk: 100rel\r\n\r\n";
        assert!(has_header(raw, "Allow"));
        assert!(has_header(raw, "Supported"), "the `k` compact form is the Supported header");
        assert!(!has_header(raw, "Require"));
        assert!(
            has_header(b"INVITE sip:x SIP/2.0\r\nSupported:\r\n\r\n", "Supported"),
            "an empty value is still an advertised header",
        );
    }

    #[test]
    fn name_addr_uri_drops_the_headers_own_parameters() {
        let raw = b"BYE sip:x SIP/2.0\r\n\
                    From: \"A\" <sip:alice@atlanta.com>;tag=at\r\n\
                    t: sip:bob@biloxi.com;tag=bt\r\n\r\n";
        assert_eq!(name_addr_uri(raw, "From").as_deref(), Some("sip:alice@atlanta.com"));
        assert_eq!(
            name_addr_uri(raw, "To").as_deref(),
            Some("sip:bob@biloxi.com"),
            "a bare addr-spec's `;tag=` is the header's parameter, not the URI's",
        );
        assert_eq!(name_addr_uri(raw, "Contact"), None, "an absent header reads None");
    }

    #[test]
    fn route_uris_splits_a_comma_fold_and_reads_loose_routing() {
        // One row, two hops; only the first advertises `;lr`, and `;lrx` is a
        // different parameter — a substring scan would call it loose.
        let raw = b"BYE sip:x SIP/2.0\r\n\
                    Route: <sip:p@host;lr>, <sip:q@h2>\r\n\
                    Route: <sip:r@h3:5080;lrx>\r\n\r\n";
        let hops = route_uris(raw, "Route").expect("every row reads");
        assert_eq!(hops.len(), 3, "{hops:?}");
        assert_eq!((hops[0].host.as_str(), hops[0].port), ("host", 5060));
        assert!(hops[0].loose);
        assert!(!hops[1].loose);
        assert_eq!((hops[2].host.as_str(), hops[2].port), ("h3", 5080));
        assert!(!hops[2].loose, "`;lrx` is not `;lr`");
        assert_eq!(
            route_uris(raw, "Record-Route"),
            Some(Vec::new()),
            "an absent header reads as no hops",
        );
        assert_eq!(
            route_uris(b"BYE sip:x SIP/2.0\r\nRoute: <sip:p@h;lr\r\n\r\n", "Route"),
            None,
            "a row no reader accepts names no hop",
        );
    }

    #[test]
    fn route_rows_locate_each_row_in_the_raw_bytes() {
        let raw = b"INVITE sip:x SIP/2.0\r\n\
                    Via: SIP/2.0/UDP 10.0.0.9:5080;branch=z9hG4bK-p\r\n\
                    Record-Route: <sip:10.0.0.9:5080;w_bak=b2;lr>, <sip:10.0.0.7;lr>\r\n\
                    CSeq: 1 INVITE\r\n\
                    Record-Route: <sip:10.0.0.9:5080;lr\r\n\
                    Content-Length: 0\r\n\r\n\
                    Record-Route: <sip:body;lr>\r\n";
        let rows = route_rows(raw, "Record-Route");
        assert_eq!(rows.len(), 2, "the body's look-alike is past the blank line: {rows:?}");
        let text = |r: &RouteRow| String::from_utf8_lossy(&raw[r.span.clone()]).into_owned();
        assert_eq!(text(&rows[0]), "Record-Route: <sip:10.0.0.9:5080;w_bak=b2;lr>, <sip:10.0.0.7;lr>\r\n");
        let hops = rows[0].hops.as_ref().expect("a readable row names its hops");
        assert_eq!(hops.len(), 2, "a comma fold is split: {hops:?}");
        assert_eq!((hops[0].host.as_str(), hops[0].port), ("10.0.0.9", 5080));
        assert_eq!((hops[1].host.as_str(), hops[1].port), ("10.0.0.7", 5060));
        assert_eq!(text(&rows[1]), "Record-Route: <sip:10.0.0.9:5080;lr\r\n");
        assert_eq!(rows[1].hops, None, "a row no reader accepts still has its span");
        assert!(route_rows(raw, "Route").is_empty(), "an absent header has no rows");
    }

    #[test]
    fn request_uri_facts_resolve_the_default_port() {
        let facts = request_uri_facts(b"BYE sip:bob@10.0.0.2 SIP/2.0\r\n\r\n").expect("reads");
        assert_eq!((facts.host.as_str(), facts.port), ("10.0.0.2", 5060));
        assert_eq!(facts.uri, "sip:bob@10.0.0.2");
        assert_eq!(
            request_uri_facts(b"SIP/2.0 200 OK\r\n\r\n"),
            None,
            "a response has no Request-URI",
        );
    }

    #[test]
    fn via_rport_keeps_the_four_states_apart() {
        let via = |v: &str| format!("OPTIONS sip:x SIP/2.0\r\nVia: {v}\r\n\r\n").into_bytes();
        assert_eq!(via_rport(&via("SIP/2.0/UDP h;branch=z9")), ViaRport::Absent);
        assert_eq!(via_rport(&via("SIP/2.0/UDP h;branch=z9;rport")), ViaRport::Requested);
        assert_eq!(
            via_rport(&via("SIP/2.0/UDP h;branch=z9;rport=5060")),
            ViaRport::Observed(5060),
        );
        assert_eq!(via_rport(&via("SIP/2.0/UDP h;branch=z9;rport=x")), ViaRport::Unreadable);
        assert_eq!(
            via_rport(b"OPTIONS sip:x SIP/2.0\r\nCall-ID: c\r\n\r\n"),
            ViaRport::Absent,
            "no Via names no rport",
        );
        let two = b"OPTIONS sip:x SIP/2.0\r\n\
                    Via: SIP/2.0/UDP top;branch=z9-t\r\n\
                    Via: SIP/2.0/UDP bot;branch=z9-b;rport=9\r\n\r\n";
        assert_eq!(via_rport(two), ViaRport::Absent, "only the top Via is read");
    }

    #[test]
    fn body_presence_is_read_off_the_header_block() {
        let head = |extra: &str| format!("SIP/2.0 183 X\r\nCSeq: 1 INVITE\r\n{extra}\r\n").into_bytes();
        assert_eq!(content_length(&head("Content-Length: 42\r\n")), Some(42));
        assert_eq!(content_length(&head("l: 7\r\n")), Some(7), "compact spelling");
        assert_eq!(content_length(&head("Content-Length: x\r\n")), None, "unreadable");
        assert_eq!(content_length(&head("")), None, "absent");

        assert!(has_body(&head("Content-Length: 42\r\n")));
        assert!(!has_body(&head("Content-Length: 0\r\n")), "§20.14: bodiless states zero");
        assert!(!has_body(&head("")), "neither field: no body declared");
        assert!(
            has_body(&head("Content-Type: application/sdp\r\n")),
            "no readable length, but the message names the body it carries",
        );
        assert!(
            !has_body(&head("Content-Length: 0\r\nContent-Type: application/sdp\r\n")),
            "a readable length settles it",
        );
    }

    #[test]
    fn body_is_what_follows_the_blank_line() {
        assert_eq!(
            body(b"INVITE sip:b SIP/2.0\r\nContent-Length: 3\r\n\r\nv=0"),
            Some(&b"v=0"[..])
        );
        assert_eq!(
            body(b"INVITE sip:b SIP/2.0\r\nContent-Length: 0\r\n\r\n"),
            Some(&b""[..]),
            "a terminated head with nothing after it carries an EMPTY body, not an unknown one"
        );
        assert_eq!(body(b"INVITE sip:b SIP/2.0\nl: 3\n\nv=0"), Some(&b"v=0"[..]), "bare LF");
        assert_eq!(body(b"INVITE sip:b SIP/2.0\r\nCSeq: 1 INVITE\r\n"), None, "unterminated");
    }
}
