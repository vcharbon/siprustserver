//! Lenient raw-datagram scanners: extract one field from an unparsed SIP datagram without a
//! full parse. For hot paths (demux, dedup, metrics labelling) that must stay cheap and
//! tolerate malformed input by returning `None`/empty rather than erroring.
//!
//! This module is the ONLY sanctioned home for raw SIP header extraction — never re-implement
//! these scanners in another crate. If a scanner you need is missing, add it here. For
//! anything richer than single-field extraction, use the real parser ([`crate::parser`]).
//! Strict, allocation-free pre-parse *classifiers* (overload brake, dispatcher fast-path) live
//! in [`crate::preparse`] and its sibling modules.

mod uri_facts;
mod via_hops;
mod via_stamp;
use std::borrow::Cow;
pub use uri_facts::UriFacts;
pub use via_hops::{bottom_via, via_rport, OriginVia, ViaRport};
pub use via_stamp::relay_stamp_spans;

use crate::char_class::is_token_char;
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
    header_value(raw, "to")
        .or_else(|| header_value(raw, "t"))
        .map_or_else(String::new, |v| tag_of(&v))
}

/// The `tag` parameter of the From header (full or compact `f` form), or the
/// empty string when absent.
pub fn from_tag(raw: &[u8]) -> String {
    header_value(raw, "from")
        .or_else(|| header_value(raw, "f"))
        .map_or_else(String::new, |v| tag_of(&v))
}

/// The `tag` parameter of one From or To value, or the empty string.
fn tag_of(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    // An ASCII fold keeps every byte offset, so `lower`'s offset is `value`'s.
    let Some(rest) = lower.find("tag=").and_then(|pos| value.get(pos + "tag=".len()..)) else {
        return String::new();
    };
    up_to_delimiter(rest, &[';', ',', ' ', '\t', '>']).trim().to_string()
}

/// `text` up to the first of `delims`, or all of it.
fn up_to_delimiter<'a>(text: &'a str, delims: &[char]) -> &'a str {
    text.split(delims).next().unwrap_or(text)
}

/// One header line split at its first colon into name and value; `None` for a
/// line holding no colon.
fn field(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut halves = line.splitn(2, |&b| b == b':');
    Some((halves.next()?, halves.next()?))
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

/// The CSeq method token as written (a method is case-sensitive, RFC 3261
/// §7.1); `None` when the CSeq is absent or carries no method.
pub fn cseq_method_token(raw: &[u8]) -> Option<String> {
    let line = cseq_line(raw)?;
    line.split_whitespace().nth(1).map(str::to_string)
}

/// The CSeq method mapped to a BOUNDED static label — safe as a low-cardinality
/// metrics label AND usable for method comparison (every RFC 3261/3262/3515
/// method maps to itself, in its own case: RFC 3261 §7.1). `"none"` when
/// absent, `"other"` for any other method.
pub fn cseq_method_label(raw: &[u8]) -> &'static str {
    let Some(l) = cseq_line(raw) else { return "none" };
    let m = l.split_whitespace().nth(1).unwrap_or("");
    match m {
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
        let value = l.get(..5).filter(|p| p.eq_ignore_ascii_case("cseq:")).and_then(|_| l.get(5..));
        if let Some(value) = value {
            return Some(value.trim().to_string());
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
        let Some((name, value)) = field(line) else { continue };
        let name = name.trim_ascii();
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
        let Some((name, value)) = field(line) else { continue };
        if !name.trim_ascii().eq_ignore_ascii_case(TRACE_SAMPLE_HEADER.as_bytes()) {
            continue;
        }
        let value = value.trim_ascii();
        if value.len() > TRACE_SAMPLE_VALUE_CAP {
            return TraceSample::Malformed;
        }
        return match std::str::from_utf8(value).ok().and_then(|v| v.parse::<f64>().ok()) {
            Some(rate) if rate.is_finite() && (0.0..=1.0).contains(&rate) => {
                TraceSample::Rate(rate)
            }
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
            let rest = v.get(v.find("branch=")? + "branch=".len()..)?;
            let b = up_to_delimiter(rest, &[';', ',', ' ', '\t']).trim();
            return (!b.is_empty()).then(|| b.to_string());
        }
    }
    None
}

/// The `branch` of every Via, top first, comma folds split (§7.3.1): the
/// transactions a request was forwarded on, its sender's own on top. A Via
/// naming no branch yields an empty entry, so positions stay those of the
/// stack.
pub fn via_branches(raw: &[u8]) -> Vec<String> {
    header_rows_compact(raw, "via")
        .iter()
        .flat_map(|row| row.split(',').map(str::to_string).collect::<Vec<_>>())
        .map(|via| {
            let Some(rest) = via.find("branch=").and_then(|pos| via.get(pos + "branch=".len()..))
            else {
                return String::new();
            };
            up_to_delimiter(rest, &[';', ' ', '\t']).trim().to_string()
        })
        .collect()
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
        if crate::parser::custom::compact_forms::expanded_name(h.trim()).eq_ignore_ascii_case(name)
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
    let start = raw.get(..offset)?.iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
    let end = raw.get(offset..)?.iter().position(|&b| b == b'\n').map_or(raw.len(), |p| offset + p);
    Some(as_str(raw.get(start..end)?).trim_end_matches('\r').to_string())
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
/// ONE head-end rule ([`head_end`]): the arm a document writes a datagram in,
/// the body layout it states and the check that reads them back all measure
/// the head here, so they cannot disagree. The declared `Content-Length` is
/// NOT applied: this returns what the datagram carried, and a consumer that
/// needs the declared length reads [`content_length`].
pub fn body(raw: &[u8]) -> Option<&[u8]> {
    head_end(raw).and_then(|end| raw.get(end..))
}

/// The offset just past the empty line that ends the header block (RFC 3261
/// §7), `None` where the datagram has none. CRLF is the line terminator
/// (§25.1); a bare CR or LF ends a line as in the parser's block split, which
/// ends the head at the same byte and then refuses it.
pub fn head_end(raw: &[u8]) -> Option<usize> {
    let at = |i: usize| raw.get(i).copied();
    let mut i = 0;
    loop {
        if i >= raw.len() {
            return None;
        }
        let line_start = i;
        while at(i).is_some_and(|b| b != b'\r' && b != b'\n') {
            i += 1;
        }
        let empty = i == line_start;
        if at(i) == Some(b'\r') {
            i += 1;
        }
        if at(i) == Some(b'\n') {
            i += 1;
        }
        if empty {
            return Some(i);
        }
        if i == line_start {
            return None;
        }
    }
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

/// The user part of the `From` URI (compact `f` included), or `None` when the
/// header is absent, no reader accepts it, or its URI names no user. The
/// display name, the URI host and parameters, and the header's own `;tag=`
/// never affect the result; the user reads as [`address_user`] reads it
/// (percent-unescaped, a `tel:` subscriber number without its parameters),
/// and compares as written: RFC 3966 visual separators (`-`, `.`, `(`, `)`)
/// are kept, so `+1-555-010` is not `+1555010`.
pub fn from_user(raw: &[u8]) -> Option<String> {
    address_user(&header_rows_compact(raw, "From").into_iter().next()?)
}

/// The URI user of an address value — a `name-addr` or `addr-spec`, as a
/// From or To header carries it, header parameters allowed — or `None` when
/// no reader accepts it or its URI names no user. The user is returned
/// percent-unescaped, the form two users compare in (RFC 3261 §19.1.4); a
/// `tel:` URI's user is its subscriber number, before any `;` parameter.
pub fn address_user(value: &str) -> Option<String> {
    use crate::header::{From as FromHeader, HeaderValue};
    let addr = FromHeader::parse(&crate::sip_str::SipStr::owned(value)).ok()?;
    let uri = addr.uri();
    let user = if uri.scheme().eq_ignore_ascii_case("tel") {
        uri.host().split(';').next().unwrap_or("")
    } else {
        uri.user()?
    };
    (!user.is_empty()).then(|| crate::param_codec::decode_param(user))
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
    while let Some(rest) = raw.get(pos..).filter(|rest| !rest.is_empty()) {
        let line_end = rest.iter().position(|&b| b == b'\n').map_or(raw.len(), |p| pos + p);
        let next = (line_end + 1).min(raw.len());
        let Some(line) = raw.get(pos..line_end).map(as_str) else { break };
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            break; // end of headers
        }
        if let Some((h, v)) = line.split_once(':') {
            if crate::parser::custom::compact_forms::expanded_name(h.trim())
                .eq_ignore_ascii_case(name)
            {
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

#[cfg(test)]
mod tests;
