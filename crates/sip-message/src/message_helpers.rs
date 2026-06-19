//! MessageHelpers — SIP header accessors + structured-header readers. Port of
//! the **pure** half of `src/sip/MessageHelpers.ts`: it reads and rewrites
//! existing headers rather than constructing new ones (construction lives in
//! [`crate::generators`]).
//!
//! The Tier-1 overload-brake byte helpers are ported ahead of slice 2 — they
//! are the allocation-free, pre-parse building blocks the future Tier-1 UDP
//! `preIngress` hook composes (see the byte-helpers section at the bottom):
//!   - [`buffer_has_emergency_marker`] — the cheap pre-parse signal the brake
//!     consults to NEVER 503 an emergency packet (pulled ahead in migration/03);
//!   - [`is_invite_request_buffer`] — does the datagram start with `INVITE `,
//!     i.e. is it a new-INVITE request the brake may shed (migration/10);
//!   - [`build_stateless_reject_503_buffer`] — byte-slices the five mandatory
//!     RFC 3261 §8.2.6.2 header lines verbatim out of an inbound INVITE and
//!     templates a stateless 503, with no parse / no txn / no To-tag
//!     (migration/10);
//!   - [`jittered_retry_after`] — jitter a `Retry-After` base by an injected
//!     roll, the seam the hook feeds an RNG into (migration/10).
//!
//! Why these four sit in a pure crate while their *consumer* does not: the
//! Tier-1 `preIngress` closure (the `UdpTransport.layer` glue in
//! `src/sip/UdpTransport.ts`) depends on `AppConfig` + `MetricsRegistry` +
//! the `sip-net` fabric — unported slices — so the closure is deferred to the
//! network slice; these helpers are the pure bytes-in/bytes-out leaves it will
//! reassemble (the `PreIngressHook`/`PreIngressAction::Reply` seam they plug
//! into already exists in `sip-net`). The end-to-end brake coverage
//! (`tests/sip/UdpTransport-brake.test.ts`) therefore stays deferred with that
//! facade; this crate pins each helper's byte-level contract directly (see the
//! `*_tests` modules below), including the three cases the brake integration
//! relies on (a non-emergency INVITE past the threshold is 503'd; an emergency
//! INVITE bypasses; a non-INVITE is never 503'd).
//!
//! The byte-level dispatcher helper [`buffer_has_to_tag`] joins the brake
//! quartet above (migration/21): a pre-parse `To`-tag scan that distinguishes an
//! **initial** request (no `To`-tag → a fresh INVITE the brake may shed) from an
//! **in-dialog** one (`To`-tag present → ACK/BYE/re-INVITE the dispatcher
//! fast-paths). Like the others it is a pure bytes-in/bool-out leaf; its
//! eventual consumer (the UDP `preIngress` dispatcher fast-path) lives in the
//! deferred network slice, so this crate pins its byte-level contract directly.
//!
//! Deferred to slice 2 (network/dispatch), NOT ported here:
//!   - the identifier generators (`newTag`/`newBranch`/`newCallId`/`currentRng`)
//!     — they read a fiber-local seeded RNG (Effect `Random`); the Rust port
//!     will inject an RNG seam at the network layer.

use std::collections::BTreeMap;

use crate::parser::custom::structured_headers::{
    parse_contact, parse_name_addr, parse_sip_uri_string, parse_via,
};
use crate::types::{ParamValue, SipHeader, SipRequest};

/// First header value matching `name` (case-insensitive).
/// Canonical top-level comma splitter (quote-, escape- AND angle-bracket-
/// aware) — the ONE implementation for header folding everywhere. Three
/// near-copies used to exist (parser x2, proxy) and two had drifted: one
/// honoured backslash escapes but split inside <...>, the other the reverse,
/// so the same value could split differently at the proxy hop than in the
/// parser that produced it.
pub use crate::parser::custom::structured_headers::split_top_level_commas;

pub fn get_header<'a>(headers: &'a [SipHeader], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

/// All header values matching `name` (case-insensitive), in wire order.
pub fn get_headers<'a>(headers: &'a [SipHeader], name: &str) -> Vec<&'a str> {
    headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
        .collect()
}

/// Set or replace a header (first occurrence). Returns a new header list.
pub fn set_header(headers: &[SipHeader], name: &str, value: &str) -> Vec<SipHeader> {
    let mut result = headers.to_vec();
    match result.iter_mut().find(|h| h.name.eq_ignore_ascii_case(name)) {
        Some(h) => {
            h.name = name.to_string();
            h.value = value.to_string();
        }
        None => result.push(SipHeader { name: name.to_string(), value: value.to_string() }),
    }
    result
}

/// Remove all headers matching `name` (case-insensitive).
pub fn remove_header(headers: &[SipHeader], name: &str) -> Vec<SipHeader> {
    headers.iter().filter(|h| !h.name.eq_ignore_ascii_case(name)).cloned().collect()
}

/// Extract the `tag` parameter from a From/To header value. Quote-aware.
pub fn extract_tag(header_value: &str) -> Option<String> {
    parse_name_addr(header_value).tag
}

/// Strip the `tag` parameter from a From/To header value, reconstructing the
/// remaining name-addr + params. Quote-aware.
pub fn strip_tag(header_value: &str) -> String {
    let parsed = parse_name_addr(header_value);
    if parsed.tag.is_none() {
        return header_value.to_string();
    }

    let mut result = String::new();
    if let Some(dn) = &parsed.display_name {
        result.push_str(&format!("\"{dn}\" "));
    }
    result.push_str(&format!("<{}>", parsed.uri));
    for (k, v) in &parsed.params {
        if k == "tag" {
            continue;
        }
        match v {
            ParamValue::Flag => result.push_str(&format!(";{k}")),
            ParamValue::Value(val) => result.push_str(&format!(";{k}={val}")),
        }
    }
    result
}

/// Extract the URI from a From/To header value (name-addr). Quote-aware.
pub fn extract_name_addr_uri(header_value: &str) -> String {
    parse_name_addr(header_value).uri
}

/// Extract the URI from a Contact header value. Quote-aware.
pub fn extract_contact_uri(contact_value: &str) -> String {
    parse_contact(contact_value).uri
}

/// Parsed SIP URI fields (port default 5060).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSipUri {
    pub scheme: String,
    pub user: Option<String>,
    pub host: String,
    pub port: u64,
    pub params: BTreeMap<String, String>,
}

/// Parse a SIP URI (angle brackets stripped if present). Zero-regex.
pub fn parse_sip_uri(uri: &str) -> Option<ParsedSipUri> {
    let mut cleaned = uri;
    if let Some(lt) = uri.find('<') {
        cleaned = match uri[lt + 1..].find('>') {
            Some(rel) => &uri[lt + 1..lt + 1 + rel],
            None => &uri[lt + 1..],
        };
    }
    let parsed = parse_sip_uri_string(cleaned)?;
    Some(ParsedSipUri {
        scheme: parsed.scheme,
        user: parsed.user,
        host: parsed.host,
        port: parsed.port.unwrap_or(5060),
        params: parsed.params,
    })
}

/// Extract host:port from a SIP URI string.
pub fn extract_host_port(uri: &str) -> Option<(String, u64)> {
    let parsed = parse_sip_uri(uri)?;
    Some((parsed.host, parsed.port))
}

/// Parse URI parameters from a SIP URI (e.g. `sip:b2bua@host;callRef=abc;leg=a`).
pub fn parse_uri_params(uri: &str) -> BTreeMap<String, String> {
    parse_sip_uri(uri).map(|p| p.params).unwrap_or_default()
}

/// The B2BUA's custom Via parameters: `branch`, `cr`, `lg`. Zero-regex.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ViaParams {
    pub branch: Option<String>,
    pub cr: Option<String>,
    pub lg: Option<String>,
}

pub fn parse_via_params(via_value: &str) -> ViaParams {
    let parsed = parse_via(via_value);
    let pick = |key: &str| match parsed.params.get(key) {
        Some(ParamValue::Value(v)) => Some(v.clone()),
        _ => None,
    };
    ViaParams { branch: parsed.branch, cr: pick("cr"), lg: pick("lg") }
}

/// Percent-encode all but RFC 3986 unreserved characters — the encoding the
/// B2BUA stamps on its Via `cr`/`lg` and Contact `callRef`/`leg` params (values
/// contain `|`/`@`/`:`, unsafe in a SIP param). The single home for this codec so
/// the encoder and its inverse [`decode_param`] cannot drift across crates (the
/// B2BUA stamps; sip-txn and the router read).
pub fn encode_param(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        if unreserved {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0x0f));
        }
    }
    out
}

/// Inverse of [`encode_param`]; invalid/truncated escapes pass through verbatim.
pub fn decode_param(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(v: u8) -> char {
    match v {
        0..=9 => (b'0' + v) as char,
        _ => (b'a' + (v - 10)) as char,
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Whether a request carries an emergency Resource-Priority header
/// (esnet.0 / wps.0 / q735.0). Case-insensitive name, case-sensitive value.
pub fn is_emergency_request(req: &SipRequest) -> bool {
    match get_header(&req.headers, "resource-priority") {
        None => false,
        Some(value) => {
            value.contains("esnet.0") || value.contains("wps.0") || value.contains("q735.0")
        }
    }
}

// ---------------------------------------------------------------------------
// Cheap byte-level classifier helpers (overload protection)
// ---------------------------------------------------------------------------
//
// These run on the RAW datagram *before* the SIP parser, in the Tier-1 UDP
// brake's hot path: they decide whether to stateless-503 an INVITE while the
// ingress queue is saturated, so they MUST be allocation-free byte scans, not
// parses. Port of the byte-classifier block in `src/sip/MessageHelpers.ts`.

/// The canonical emergency Resource-Priority namespace.value tokens (per
/// docs/overload-protection.md). Matched case-SENSITIVELY — the upstream
/// contract requires canonical casing, mirroring [`is_emergency_request`].
const EMERGENCY_RPH_TOKENS: [&[u8]; 3] = [b"esnet.0", b"wps.0", b"q735.0"];

/// Cheap byte scan: does the raw datagram carry an emergency signal?
///
/// Two signals, checked in TS order (cheapest first):
///   1. the dispatcher-side markers `;emerg=1` (Request-URI) or `;em=1` (Via
///      custom param) that the B2BUA stamps once an emergency call is admitted
///      (see `b2bua::stack_identity`), which every subsequent in-dialog packet
///      then carries — a plain substring match anywhere in the buffer; then
///   2. an **initial** INVITE's `Resource-Priority:` header (case-sensitive
///      canonical name) whose value, within that same header line, contains one
///      of the canonical emergency tokens [`EMERGENCY_RPH_TOKENS`].
///
/// This is the exact signal the Tier-1 brake consults to NEVER 503 an emergency
/// packet. It operates on bytes (no UTF-8/parse cost) and never allocates.
pub fn buffer_has_emergency_marker(raw: &[u8]) -> bool {
    // Cheap path: dispatcher-side markers stamped on admitted calls.
    if find_subslice(raw, b";emerg=1").is_some() {
        return true;
    }
    if find_subslice(raw, b";em=1").is_some() {
        return true;
    }

    // Initial INVITE: Resource-Priority header (case-sensitive canonical name).
    let rp_idx = match find_subslice(raw, b"Resource-Priority:") {
        Some(idx) => idx,
        None => return false,
    };
    // Confine the token scan to that header's own line: from the match to the
    // next CRLF (or end of buffer if the line is unterminated), so a token in a
    // later header / the body cannot spoof an emergency.
    let line_end = find_subslice(&raw[rp_idx..], b"\r\n").map(|rel| rp_idx + rel);
    let slice = match line_end {
        Some(end) => &raw[rp_idx..end],
        None => &raw[rp_idx..],
    };
    EMERGENCY_RPH_TOKENS.iter().any(|tok| find_subslice(slice, tok).is_some())
}

/// Quick byte check: does the datagram start with `INVITE ` — i.e. is this a
/// new-INVITE request line, decidable before any parse? Returns `false` for
/// ACK/BYE/CANCEL/OPTIONS/… and for responses (`SIP/2.0 …`). Port of
/// `isInviteRequestBuffer`: a fixed seven-byte prefix compare
/// (`I N V I T E SP` = `0x49 0x4E 0x56 0x49 0x54 0x45 0x20`), so a too-short
/// buffer is trivially not an INVITE.
///
/// Case-SENSITIVE, matching the TS: SIP method names are upper-case on the wire
/// (RFC 3261 §7.1), and this runs in the brake's hot path where a canonical
/// compare is the whole point — a lower-case `invite` is not a method token the
/// brake sheds.
pub fn is_invite_request_buffer(raw: &[u8]) -> bool {
    raw.starts_with(b"INVITE ")
}

/// Cheap byte scan: does the datagram carry a `To`-tag (`;tag=` on the `To`
/// header line)? Port of `bufferHasToTag`. The pre-parse discriminator the
/// dispatcher fast-path uses to tell an **initial** request (no `To`-tag — a
/// fresh INVITE) from an **in-dialog** one (`To`-tag present — ACK / BYE /
/// re-INVITE inside an established dialog), decidable before any SIP parse.
///
/// Walks the header section line by line (CRLF-delimited) until it finds the
/// `To` header, then scans that **one physical line** for `;tag=`:
///   - the `To` header must sit at a **line start** (so a `;tag=` inside, say, a
///     `History-Info` value cannot be mistaken for the dialog tag), matched
///     case-SENSITIVELY against the canonical `To`/`t` (RFC 3261 §7.1 compact
///     form `t`) followed by `:` or SP — the same canonical-casing contract as
///     [`is_invite_request_buffer`] / the emergency classifiers;
///   - the scan stops at the **first** `To` line it reaches: if that line has no
///     `;tag=`, the result is `false` even though later lines are not inspected
///     (a well-formed message has exactly one `To`);
///   - the walk ends at the blank line that terminates the header section
///     (`false`), and at an unterminated final line / missing CRLF (`false`) —
///     so a `To`-tag must be on a properly CRLF-terminated `To` line to count.
///
/// Allocation-free; operates on raw bytes (no UTF-8 / parse cost).
pub fn buffer_has_to_tag(raw: &[u8]) -> bool {
    let mut idx = 0;
    while idx < raw.len() {
        let line_start = idx;
        // No CRLF from here on → no terminated line left to inspect.
        let line_end = match find_subslice(&raw[line_start..], b"\r\n") {
            Some(rel) => line_start + rel,
            None => return false,
        };
        // A zero-length line is the blank line that ends the header section.
        if line_end == line_start {
            return false;
        }

        // Match a `To`/`t` header at the line start, case-sensitively, in its
        // canonical and compact forms. `.get()` keeps the lookahead in bounds:
        // the TS reads `raw[lineStart + n]` and relies on JS returning
        // `undefined` past the end (never equal to ':'/' '); the Rust analogue
        // is a checked index that simply does not match.
        let c0 = raw[line_start];
        let is_to_line = if c0 == b'T' && raw.get(line_start + 1) == Some(&b'o') {
            matches!(raw.get(line_start + 2), Some(&b':') | Some(&b' '))
        } else if c0 == b't' {
            matches!(raw.get(line_start + 1), Some(&b':') | Some(&b' '))
        } else {
            false
        };
        if is_to_line {
            // Confine the `;tag=` scan to this one physical `To` line.
            return find_subslice(&raw[line_start..line_end], b";tag=").is_some();
        }

        idx = line_end + 2;
    }
    false
}

/// Compute a jittered `Retry-After` value (seconds).
///
/// Port of `jitteredRetryAfter`. The TS draws from `Math.random()` directly
/// because its sole caller is the Tier-1 UDP overload pre-ingress hook, which
/// runs outside any Effect fiber — overload-protection nondeterminism is
/// explicitly out of scope for the seeded-`Random` plumbing. A *pure* crate
/// cannot reach for a global RNG, so the randomness is **injected**: `roll`
/// yields a fresh value in `[0, u64::MAX]` (e.g. `rand::random::<u64>()` at the
/// network-layer call site, exactly where this module's other RNG seam lands).
/// This keeps the function deterministic and unit-testable while preserving the
/// TS arithmetic exactly.
///
/// Returns `base_sec` unchanged when `jitter_sec == 0` (the
/// `retryAfterJitterSec: 0` config the brake tests pin), otherwise
/// `base_sec + (roll mod (jitter_sec + 1))` — a uniform offset in the inclusive
/// range `[0, jitter_sec]`, mirroring TS `Math.floor(Math.random() * (jitter +
/// 1))`.
pub fn jittered_retry_after(base_sec: u32, jitter_sec: u32, roll: impl FnOnce() -> u64) -> u32 {
    if jitter_sec == 0 {
        return base_sec;
    }
    // `jitter_sec + 1` fits in u64 (jitter_sec: u32); the modulus is in
    // [0, jitter_sec] so the sum cannot exceed base_sec + jitter_sec.
    let offset = (roll() % (u64::from(jitter_sec) + 1)) as u32;
    base_sec + offset
}

/// Build a stateless **503 Service Unavailable** response by byte-slicing the
/// mandatory header lines verbatim out of an inbound INVITE datagram — no SIP
/// parse, no transaction allocation, no `To`-tag. Port of
/// `buildStatelessReject503Buffer`: the Tier-1 (UDP `preIngress`) cheap-rejection
/// path that sheds load before the parser ever runs.
///
/// Per RFC 3261 §8.2.6.2 the five headers a UAS MUST copy into a response are
/// taken verbatim from the request (first occurrence of each; compact forms
/// `v`/`f`/`t`/`i` accepted on input):
///   - `Via` (topmost — the UAC matches the response on it),
///   - `From` (echoed),
///   - `To` (echoed **without** adding our own tag — see below),
///   - `Call-ID`,
///   - `CSeq`.
///
/// We deliberately add **no** `To`-tag. The UAC's resulting ACK therefore
/// carries no dialog context, so it matches nothing in the transaction layer's
/// dialog index and is dropped at the orphan-ACK rule — that is the
/// cheap-rejection contract (and why this is distinct from the Tier-3
/// `b2bua::router::build_stateless_overload_503`, which runs *after* a parse,
/// reuses the full `generate_response` machinery, and DOES add a `To`-tag).
///
/// Returns `None` when the buffer does not look like a SIP **request** we can
/// template (no header terminator, fewer than two lines, a non-request first
/// line, or any of the five required headers missing) — the caller then drops
/// the packet silently (`PreIngressAction::Accept`, letting the normal pipeline
/// reject) rather than emitting a malformed reply.
///
/// Output header names are normalised to canonical casing; the value (after the
/// first `:`) is copied byte-for-byte from the request line. The reply is
/// CRLF-terminated regardless of whether the request used LF-only separators.
pub fn build_stateless_reject_503_buffer(raw: &[u8], retry_after_sec: u32) -> Option<Vec<u8>> {
    // Locate the header-section terminator: CRLFCRLF, or LFLF as a fallback
    // (mirrors the TS `indexOf("\r\n\r\n")` then `indexOf("\n\n")`). The chosen
    // terminator dictates the intra-section line separator.
    let (header_end, line_sep): (usize, &[u8]) = match find_subslice(raw, b"\r\n\r\n") {
        Some(end) => (end, b"\r\n"),
        None => match find_subslice(raw, b"\n\n") {
            Some(end) => (end, b"\n"),
            None => return None,
        },
    };

    // The header section is ASCII on the wire; lossy decode keeps the verbatim
    // bytes for the value copy (header names/values the brake sees are ASCII).
    let header_section = &raw[..header_end];
    let lines: Vec<&[u8]> = split_on(header_section, line_sep);
    if lines.len() < 2 {
        return None;
    }

    // First line must be a request line — we only template responses to
    // requests (a response first line is `SIP/2.0 <code> …`; the TS keys off the
    // mere presence of "SIP/2.0", and a request line ends with it, so the same
    // cheap substring test stands in for "looks like a SIP message"). Absent →
    // bail (`?` discards the match index; we only care that it is present).
    find_subslice(lines[0], b"SIP/2.0")?;

    let mut via_line: Option<&[u8]> = None;
    let mut from_line: Option<&[u8]> = None;
    let mut to_line: Option<&[u8]> = None;
    let mut call_id_line: Option<&[u8]> = None;
    let mut cseq_line: Option<&[u8]> = None;

    for line in &lines[1..] {
        if line.is_empty() {
            continue;
        }
        // A continuation line (RFC 3261 line folding — starts with SP/HTAB)
        // belongs to the previous header; the verbatim single-line copy is the
        // contract, so skip it (matches the TS `charCodeAt(0)` guard).
        if line[0] == b' ' || line[0] == b'\t' {
            continue;
        }
        let colon = match find_subslice(line, b":") {
            Some(c) => c,
            None => continue,
        };
        // Header-name match is case-INSENSITIVE (RFC 3261 §7.3.1; the TS lower-
        // cases the trimmed name), unlike the case-sensitive *value* scans in
        // the emergency classifiers. The `if ….is_none()` guard on each arm is
        // the "first occurrence wins" contract (a later duplicate is ignored).
        let name = trim_ascii(&line[..colon]).to_ascii_lowercase();
        match name.as_slice() {
            b"via" | b"v" if via_line.is_none() => via_line = Some(line),
            b"from" | b"f" if from_line.is_none() => from_line = Some(line),
            b"to" | b"t" if to_line.is_none() => to_line = Some(line),
            b"call-id" | b"i" if call_id_line.is_none() => call_id_line = Some(line),
            b"cseq" if cseq_line.is_none() => cseq_line = Some(line),
            _ => {}
        }

        if via_line.is_some()
            && from_line.is_some()
            && to_line.is_some()
            && call_id_line.is_some()
            && cseq_line.is_some()
        {
            break;
        }
    }

    let (via_line, from_line, to_line, call_id_line, cseq_line) =
        match (via_line, from_line, to_line, call_id_line, cseq_line) {
            (Some(v), Some(f), Some(t), Some(i), Some(c)) => (v, f, t, i, c),
            _ => return None,
        };

    // Emit the response with canonical header names; the value (everything from
    // the first `:` onward, including the colon and any surrounding whitespace)
    // is copied byte-for-byte from the request line.
    let mut out: Vec<u8> = Vec::with_capacity(raw.len() + 80);
    out.extend_from_slice(b"SIP/2.0 503 Service Unavailable\r\n");
    push_normalized_header(&mut out, b"Via", via_line);
    push_normalized_header(&mut out, b"From", from_line);
    push_normalized_header(&mut out, b"To", to_line);
    push_normalized_header(&mut out, b"Call-ID", call_id_line);
    push_normalized_header(&mut out, b"CSeq", cseq_line);
    out.extend_from_slice(b"Reason: SIP;cause=503;text=\"overload\"\r\n");
    out.extend_from_slice(b"Retry-After: ");
    out.extend_from_slice(retry_after_sec.to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(b"Content-Length: 0\r\n\r\n");
    Some(out)
}

/// Append `canonical_name` + the verbatim value tail of `line` (from its first
/// `:` onward) + CRLF. Mirrors the TS `normalizeHeaderLine`: keep the inbound
/// value bytes exactly (colon, spacing, params), swap only the header name for
/// its canonical casing. `line` is a header line known to contain a `:`.
fn push_normalized_header(out: &mut Vec<u8>, canonical_name: &[u8], line: &[u8]) {
    let colon = find_subslice(line, b":").expect("caller passes a line containing ':'");
    out.extend_from_slice(canonical_name);
    out.extend_from_slice(&line[colon..]);
    out.extend_from_slice(b"\r\n");
}

/// Split `data` on every occurrence of `sep`, returning the (possibly empty)
/// pieces between separators — the byte analogue of TS `String.split(sep)`
/// (so `"a||b".split("|")` ⇒ `["a", "", "b"]`, and a trailing separator yields a
/// trailing empty piece). `sep` is always non-empty here.
fn split_on<'a>(data: &'a [u8], sep: &[u8]) -> Vec<&'a [u8]> {
    let mut pieces = Vec::new();
    let mut start = 0;
    while let Some(rel) = find_subslice(&data[start..], sep) {
        let at = start + rel;
        pieces.push(&data[start..at]);
        start = at + sep.len();
    }
    pieces.push(&data[start..]);
    pieces
}

/// Trim leading/trailing ASCII whitespace from a byte slice (the byte analogue
/// of `str::trim` over the header-name slice). Avoids a UTF-8 round-trip in the
/// brake hot path. (`u8::is_ascii_whitespace` matches space, tab, LF, FF, CR.)
fn trim_ascii(mut s: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = s {
        if first.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = s {
        if last.is_ascii_whitespace() {
            s = rest;
        } else {
            break;
        }
    }
    s
}

/// First index of `needle` within `haystack` (byte substring search), or
/// `None`. The Rust equivalent of `Buffer.indexOf` used by the byte
/// classifiers. Empty `needle` matches at 0 (as `indexOf("")` does), matching
/// the TS contract; the callers here never pass an empty needle.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod emergency_tests {
    //! Port of the documented `isEmergencyRequest` contract from
    //! `src/sip/MessageHelpers.ts` (L379–387). The TS function has no dedicated
    //! unit test in the source suite — it is exercised end-to-end by the proxy
    //! admission (`selectForNewDialog-overload.test.ts`) and the byte-level UDP
    //! brake (`UdpTransport-brake.test.ts`, which tests the *separate*
    //! `bufferHasEmergencyMarker` helper, deferred per this module's doc). These
    //! tests pin the contract the TS implementation encodes: case-INsensitive
    //! header lookup, case-SENSITIVE value match (canonical casing required by
    //! the upstream contract, per docs/overload-protection.md), substring match
    //! against the three canonical RPH tokens, `false` when absent.

    use super::is_emergency_request;
    use crate::parser::SipParser;
    use crate::parser::custom::CustomParser;
    use crate::types::SipMessage;

    /// Parse a minimal INVITE carrying `rph` as the `Resource-Priority` header
    /// value. `rph == None` omits the header entirely.
    fn invite_with_rph(header_name: Option<&str>, value: Option<&str>) -> crate::types::SipRequest {
        let rph_line = match (header_name, value) {
            (Some(name), Some(v)) => format!("{name}: {v}\r\n"),
            _ => String::new(),
        };
        let raw = format!(
            "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-emerg\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=emerg-from\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: emerg-call@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
{rph_line}\
Content-Length: 0\r\n\r\n"
        );
        match CustomParser::new().parse(raw.as_bytes()).expect("fixture INVITE should parse") {
            SipMessage::Request(r) => r,
            SipMessage::Response(_) => panic!("expected request"),
        }
    }

    #[test]
    fn each_canonical_token_is_emergency() {
        for tok in ["esnet.0", "wps.0", "q735.0"] {
            let req = invite_with_rph(Some("Resource-Priority"), Some(tok));
            assert!(is_emergency_request(&req), "{tok} should be emergency");
        }
    }

    #[test]
    fn header_name_lookup_is_case_insensitive() {
        // Lower-cased header name still matched (getHeader is case-insensitive).
        let req = invite_with_rph(Some("resource-priority"), Some("esnet.0"));
        assert!(is_emergency_request(&req));
    }

    #[test]
    fn value_match_is_case_sensitive() {
        // Canonical casing is required by the contract — an upper-cased token
        // must NOT be treated as emergency.
        let req = invite_with_rph(Some("Resource-Priority"), Some("ESNET.0"));
        assert!(!is_emergency_request(&req));
    }

    #[test]
    fn absent_header_is_not_emergency() {
        let req = invite_with_rph(None, None);
        assert!(!is_emergency_request(&req));
    }

    #[test]
    fn non_emergency_priority_value_is_not_emergency() {
        // A well-formed but non-emergency RPH namespace.value.
        let req = invite_with_rph(Some("Resource-Priority"), Some("dsn.flash"));
        assert!(!is_emergency_request(&req));
    }

    #[test]
    fn token_matches_as_substring_among_multiple() {
        // The TS uses indexOf !== -1, so a canonical token embedded in a
        // multi-namespace RPH value still flags emergency.
        let req = invite_with_rph(Some("Resource-Priority"), Some("dsn.flash, q735.0"));
        assert!(is_emergency_request(&req));
    }
}

#[cfg(test)]
mod buffer_emergency_tests {
    //! Port of the `bufferHasEmergencyMarker` contract from
    //! `src/sip/MessageHelpers.ts` (L421–438). Like its parsed sibling
    //! `isEmergencyRequest`, the TS function has **no dedicated unit test** in
    //! the source suite — it is exercised only end-to-end by the byte-level UDP
    //! brake (`tests/sip/UdpTransport-brake.test.ts`), which cannot be ported
    //! until the `UdpTransport` facade lands in slice 2 (network/dispatch).
    //!
    //! These tests therefore pin the byte-scan contract the TS implementation
    //! encodes directly, including the three cases the brake integration relies
    //! on (an `esnet.0` RPH INVITE bypasses the brake; a plain INVITE does not):
    //!   - the stamped in-dialog markers `;emerg=1` (Request-URI) / `;em=1`
    //!     (Via) match **anywhere** in the datagram — the cheap path checked
    //!     first;
    //!   - the **initial** INVITE path matches a case-SENSITIVE
    //!     `Resource-Priority:` header name whose **same-line** value carries a
    //!     canonical token (`esnet.0` / `wps.0` / `q735.0`);
    //!   - a token on a *different* line than the `Resource-Priority:` header
    //!     does NOT count (line-confined scan);
    //!   - `false` when no signal is present.
    //!
    //! The marker strings asserted here are exactly what `b2bua::stack_identity`
    //! stamps: `em=1` as a Via custom param and `emerg=1` as a Request-URI
    //! param, which serialise to `;em=1` / `;emerg=1` on the wire.

    use super::buffer_has_emergency_marker;

    /// Build a minimal INVITE datagram. `rph` is the optional
    /// `Resource-Priority` header *value*; `name` lets a test vary the header
    /// casing to pin case-sensitivity.
    fn invite_buf(rph: Option<(&str, &str)>) -> Vec<u8> {
        let rph_line = match rph {
            Some((name, value)) => format!("{name}: {value}\r\n"),
            None => String::new(),
        };
        format!(
            "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-brake-0\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag-0\r\n\
To: <sip:bob@b2bua.test>\r\n\
Call-ID: brake-test-0@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5555>\r\n\
Max-Forwards: 70\r\n\
{rph_line}\
Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn each_canonical_rph_token_flags_emergency() {
        // Mirrors `buildInviteBuffer(_, { emergency: true })` (esnet.0) plus the
        // other two canonical tokens.
        for tok in ["esnet.0", "wps.0", "q735.0"] {
            let buf = invite_buf(Some(("Resource-Priority", tok)));
            assert!(buffer_has_emergency_marker(&buf), "{tok} should flag emergency");
        }
    }

    #[test]
    fn plain_invite_is_not_emergency() {
        // The non-emergency `buildInviteBuffer` shape — must NOT bypass the brake.
        assert!(!buffer_has_emergency_marker(&invite_buf(None)));
    }

    #[test]
    fn rph_header_name_is_case_sensitive() {
        // TS scans for the literal "Resource-Priority:" — a lower-cased header
        // name is not matched (canonical-casing contract).
        let buf = invite_buf(Some(("resource-priority", "esnet.0")));
        assert!(!buffer_has_emergency_marker(&buf));
    }

    #[test]
    fn rph_token_match_is_case_sensitive() {
        // Canonical token casing required: an upper-cased token does not flag.
        let buf = invite_buf(Some(("Resource-Priority", "ESNET.0")));
        assert!(!buffer_has_emergency_marker(&buf));
    }

    #[test]
    fn non_emergency_rph_value_is_not_emergency() {
        let buf = invite_buf(Some(("Resource-Priority", "dsn.flash")));
        assert!(!buffer_has_emergency_marker(&buf));
    }

    #[test]
    fn rph_token_matches_as_substring_among_multiple_namespaces() {
        // indexOf semantics: a canonical token embedded in a multi-namespace
        // value still flags.
        let buf = invite_buf(Some(("Resource-Priority", "dsn.flash, q735.0")));
        assert!(buffer_has_emergency_marker(&buf));
    }

    #[test]
    fn token_on_a_different_line_than_rph_does_not_flag() {
        // A canonical token elsewhere in the message (here a Subject header)
        // must NOT count — the scan is confined to the Resource-Priority line.
        // The RP header itself carries a non-emergency value.
        let raw = "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-x\r\n\
Subject: priority esnet.0 please\r\n\
Resource-Priority: dsn.flash\r\n\
Content-Length: 0\r\n\r\n";
        assert!(!buffer_has_emergency_marker(raw.as_bytes()));
    }

    #[test]
    fn unterminated_rph_line_still_scans_to_end() {
        // No trailing CRLF after the value (lineEnd === -1 branch in TS):
        // subarray(rpIdx) scans to the end and still finds the token.
        let raw = b"INVITE sip:bob SIP/2.0\r\nResource-Priority: wps.0";
        assert!(buffer_has_emergency_marker(raw));
    }

    #[test]
    fn stamped_via_em_marker_flags_anywhere() {
        // `;em=1` is the Via custom param b2bua::stack_identity stamps; it sits
        // on the Via line, not in the Request-URI — must match anywhere.
        let raw = b"BYE sip:bob@127.0.0.1 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-9;em=1\r\n\
Content-Length: 0\r\n\r\n";
        assert!(buffer_has_emergency_marker(raw));
    }

    #[test]
    fn stamped_requri_emerg_marker_flags_anywhere() {
        // `;emerg=1` is the Request-URI param stamped on admitted emergency
        // calls; a later in-dialog request (no RPH header at all) carries it.
        let raw = b"ACK sip:b2bua@127.0.0.1:5060;emerg=1 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-3\r\n\
Content-Length: 0\r\n\r\n";
        assert!(buffer_has_emergency_marker(raw));
    }

    #[test]
    fn em_marker_is_checked_before_rph_so_non_invite_in_dialog_passes() {
        // An in-dialog OPTIONS with neither RPH nor markers is not emergency
        // (sanity: the cheap path returns false, then no RPH header → false).
        let raw = b"OPTIONS sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-opt\r\n\
Content-Length: 0\r\n\r\n";
        assert!(!buffer_has_emergency_marker(raw));
    }

    #[test]
    fn empty_buffer_is_not_emergency() {
        assert!(!buffer_has_emergency_marker(b""));
    }

    #[test]
    fn short_buffer_below_needle_length_is_not_emergency() {
        // Guards the `needle.len() > haystack.len()` early-out in find_subslice.
        assert!(!buffer_has_emergency_marker(b"INV"));
    }
}

#[cfg(test)]
mod find_subslice_tests {
    use super::find_subslice;

    #[test]
    fn finds_at_start_middle_and_absent() {
        assert_eq!(find_subslice(b"abcdef", b"abc"), Some(0));
        assert_eq!(find_subslice(b"abcdef", b"cd"), Some(2));
        assert_eq!(find_subslice(b"abcdef", b"xyz"), None);
    }

    #[test]
    fn empty_needle_matches_at_zero_like_indexof() {
        assert_eq!(find_subslice(b"abc", b""), Some(0));
        assert_eq!(find_subslice(b"", b""), Some(0));
    }

    #[test]
    fn needle_longer_than_haystack_is_none() {
        assert_eq!(find_subslice(b"ab", b"abc"), None);
    }
}

#[cfg(test)]
mod codec_tests {
    use super::{decode_param, encode_param};

    #[test]
    fn param_round_trips_through_unsafe_chars() {
        let raw = "w0|alice@example.com:5060|ab12cd34";
        let enc = encode_param(raw);
        assert!(!enc.contains('|') && !enc.contains('@') && !enc.contains(':'));
        assert_eq!(decode_param(&enc), raw, "encode∘decode is identity");
    }

    #[test]
    fn decode_passes_truncated_escapes_verbatim() {
        assert_eq!(decode_param("ab%"), "ab%");
        assert_eq!(decode_param("ab%4"), "ab%4");
        assert_eq!(decode_param("%zz"), "%zz");
    }
}

#[cfg(test)]
mod is_invite_request_buffer_tests {
    //! Port of the `isInviteRequestBuffer` contract from
    //! `src/sip/MessageHelpers.ts` (L397–409). No dedicated TS unit test — it is
    //! exercised end-to-end by the Tier-1 brake (`UdpTransport-brake.test.ts`,
    //! "non-INVITE requests are not 503'd by the brake", which relies on an
    //! INVITE classifying true and an OPTIONS classifying false). These pin the
    //! byte-prefix contract directly: the exact seven-byte `INVITE ` prefix,
    //! case-sensitive, and false for every other method / response / short
    //! buffer.

    use super::is_invite_request_buffer;

    #[test]
    fn invite_request_line_is_an_invite() {
        // The exact shape the brake's `buildInviteBuffer` produces.
        let raw = b"INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\nVia: SIP/2.0/UDP x\r\n\r\n";
        assert!(is_invite_request_buffer(raw));
    }

    #[test]
    fn bare_invite_space_is_the_minimal_match() {
        // Exactly the 7-byte prefix, nothing after — still an INVITE start.
        assert!(is_invite_request_buffer(b"INVITE "));
    }

    #[test]
    fn other_methods_are_not_invites() {
        // The brake only sheds new INVITEs; ACK/BYE/CANCEL/OPTIONS pass.
        for line in [
            &b"OPTIONS sip:bob SIP/2.0\r\n"[..],
            &b"ACK sip:bob SIP/2.0\r\n"[..],
            &b"BYE sip:bob SIP/2.0\r\n"[..],
            &b"CANCEL sip:bob SIP/2.0\r\n"[..],
            &b"REGISTER sip:bob SIP/2.0\r\n"[..],
        ] {
            assert!(!is_invite_request_buffer(line), "{line:?} must not be an INVITE");
        }
    }

    #[test]
    fn response_is_not_an_invite_request() {
        assert!(!is_invite_request_buffer(b"SIP/2.0 200 OK\r\n"));
    }

    #[test]
    fn invite_without_trailing_space_does_not_match() {
        // "INVITEX ..." is not the INVITE method token — the trailing space is
        // load-bearing (it is byte 7 of the compare).
        assert!(!is_invite_request_buffer(b"INVITEX sip:bob SIP/2.0\r\n"));
    }

    #[test]
    fn lower_case_invite_does_not_match() {
        // Case-sensitive: method tokens are upper-case on the wire.
        assert!(!is_invite_request_buffer(b"invite sip:bob SIP/2.0\r\n"));
    }

    #[test]
    fn too_short_buffers_are_not_invites() {
        // Guards the `len < 7` early-out (here: the empty buffer and a 6-byte
        // "INVITE" with no following space).
        assert!(!is_invite_request_buffer(b""));
        assert!(!is_invite_request_buffer(b"INVITE"));
    }
}

#[cfg(test)]
mod buffer_has_to_tag_tests {
    //! Port of the `bufferHasToTag` contract from `src/sip/MessageHelpers.ts`
    //! (L444–473). No dedicated TS unit test exists — the source suite never
    //! tests this byte helper directly (the `To`-tag / new-dialog distinction is
    //! tested elsewhere only through the *parsed* path, e.g.
    //! `ProxyCore.ts`'s `req.getHeader("to").tag` and the RFC cross-message
    //! rules), and its future consumer (the UDP `preIngress` dispatcher
    //! fast-path) lives in the deferred network slice. These tests therefore pin
    //! the byte-scan contract the TS implementation encodes directly:
    //!   - a `;tag=` on the `To` line ⇒ `true` (in-dialog); its absence ⇒
    //!     `false` (initial request);
    //!   - the `To` header must be at a **line start** — a `;tag=` inside a
    //!     non-`To` header (e.g. `History-Info`) does NOT count;
    //!   - case-SENSITIVE canonical `To`, plus the compact form `t`, each in the
    //!     `<name>:` and `<name> ` shapes;
    //!   - the scan stops at the **first** `To` line (a tagless `To` ⇒ `false`
    //!     even if a later line carried `;tag=`);
    //!   - the header-terminating blank line and an unterminated / CRLF-less
    //!     buffer both stop the walk ⇒ `false`.

    use super::buffer_has_to_tag;

    /// Build a minimal INVITE datagram whose `To` header line is exactly `to`
    /// (passed without the trailing CRLF, which this helper appends).
    fn invite_with_to_line(to: &str) -> Vec<u8> {
        format!(
            "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-tt\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag\r\n\
{to}\r\n\
Call-ID: tt-call@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn to_tag_present_is_in_dialog() {
        // The in-dialog shape: a `To` carrying `;tag=` (an ACK/BYE/re-INVITE).
        let buf = invite_with_to_line("To: <sip:bob@b2bua.test>;tag=bob-dialog-tag");
        assert!(buffer_has_to_tag(&buf));
    }

    #[test]
    fn no_to_tag_is_initial_request() {
        // The initial-INVITE shape: a bare `To` with no `;tag=`.
        let buf = invite_with_to_line("To: <sip:bob@b2bua.test>");
        assert!(!buffer_has_to_tag(&buf));
    }

    #[test]
    fn compact_to_form_with_colon_is_recognised() {
        // RFC 3261 §7.1 compact form `t:` at line start.
        let buf = invite_with_to_line("t: <sip:bob@b2bua.test>;tag=compact-colon");
        assert!(buffer_has_to_tag(&buf));
    }

    #[test]
    fn compact_to_form_with_space_is_recognised() {
        // `t ` (space instead of colon) — the second accepted separator.
        let buf = invite_with_to_line("t <sip:bob@b2bua.test>;tag=compact-space");
        assert!(buffer_has_to_tag(&buf));
    }

    #[test]
    fn full_to_form_with_space_separator_is_recognised() {
        // `To ` (space separator) on the full header name.
        let buf = invite_with_to_line("To <sip:bob@b2bua.test>;tag=full-space");
        assert!(buffer_has_to_tag(&buf));
    }

    #[test]
    fn tag_inside_a_non_to_header_does_not_count() {
        // A `;tag=` embedded in a non-`To` header (here History-Info) must NOT
        // be mistaken for the dialog tag — the To must be at a line start. The
        // actual `To` line carries no tag, so the result is `false`.
        let raw = "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-hi\r\n\
History-Info: <sip:carol@x>;index=1;tag=not-a-dialog-tag\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag\r\n\
To: <sip:bob@b2bua.test>\r\n\
Call-ID: hi-call@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
        assert!(!buffer_has_to_tag(raw.as_bytes()));
    }

    #[test]
    fn to_header_name_match_is_case_sensitive() {
        // Canonical casing required: a lower-cased `to:` (which is not the
        // compact `t` form either) is not matched as the To header, so its
        // `;tag=` is never inspected ⇒ `false`.
        let buf = invite_with_to_line("to: <sip:bob@b2bua.test>;tag=lowercased");
        assert!(!buffer_has_to_tag(&buf));
    }

    #[test]
    fn first_to_line_decides_even_when_tagless() {
        // The scan returns on the FIRST To line it reaches. A tagless To here
        // yields `false` even though a (malformed) second To line below carries
        // a tag — the TS returns from the first To match unconditionally.
        let raw = "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-dup\r\n\
To: <sip:bob@b2bua.test>\r\n\
To: <sip:bob@b2bua.test>;tag=second-to-tag\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag\r\n\
Call-ID: dup-call@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
        assert!(!buffer_has_to_tag(raw.as_bytes()));
    }

    #[test]
    fn tag_only_in_body_after_header_terminator_does_not_count() {
        // The walk stops at the blank line ending the header section, so a
        // `To: ...;tag=` that appears only in the *body* is never reached.
        let raw = "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-body\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag\r\n\
Call-ID: body-call@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Content-Type: text/plain\r\n\
Content-Length: 28\r\n\r\n\
To: <sip:x@y>;tag=in-the-body";
        assert!(!buffer_has_to_tag(raw.as_bytes()));
    }

    #[test]
    fn buffer_without_crlf_is_not_in_dialog() {
        // No CRLF anywhere (lineEnd === -1 on the first iteration) ⇒ `false`,
        // even though a `;tag=` is present — a To-tag must be on a properly
        // CRLF-terminated To line.
        assert!(!buffer_has_to_tag(b"To: <sip:bob>;tag=no-crlf"));
    }

    #[test]
    fn unterminated_to_line_at_end_is_not_in_dialog() {
        // The To line is the final line with no trailing CRLF: the walk reaches
        // it but `find("\r\n")` from there is None ⇒ `false` (the TS `lineEnd
        // === -1` guard inside the loop). The earlier lines ARE terminated.
        let raw = b"INVITE sip:bob SIP/2.0\r\n\
Via: SIP/2.0/UDP h;branch=z9hG4bK-u\r\n\
To: <sip:bob@b2bua.test>;tag=unterminated";
        assert!(!buffer_has_to_tag(raw));
    }

    #[test]
    fn empty_buffer_is_not_in_dialog() {
        assert!(!buffer_has_to_tag(b""));
    }

    #[test]
    fn blank_line_first_is_not_in_dialog() {
        // A leading CRLF makes the very first line zero-length (the header
        // terminator) ⇒ `false` immediately.
        assert!(!buffer_has_to_tag(b"\r\nTo: <sip:bob>;tag=after-blank\r\n\r\n"));
    }

    #[test]
    fn to_at_buffer_end_without_separator_does_not_panic() {
        // A truncated buffer that ends exactly at "To" / "t" exercises the
        // bounds-checked lookahead (`raw.get(line_start + n)`): the TS relies on
        // JS `undefined`; the Rust port must not index out of range. A
        // CRLF-terminated bare "To" line carries no `;tag=` ⇒ `false`.
        assert!(!buffer_has_to_tag(b"To\r\n"));
        assert!(!buffer_has_to_tag(b"t\r\n"));
        // And with no trailing CRLF at all (unterminated) ⇒ `false`.
        assert!(!buffer_has_to_tag(b"To"));
        assert!(!buffer_has_to_tag(b"T"));
    }
}

#[cfg(test)]
mod jittered_retry_after_tests {
    //! Port of the `jitteredRetryAfter` contract from
    //! `src/sip/MessageHelpers.ts` (L366–369). No dedicated TS unit test — it is
    //! exercised by the Tier-1 brake with `retryAfterJitterSec: 0` (the
    //! `UdpTransport-brake.test.ts` config), i.e. the zero-jitter identity path.
    //! These pin both the identity path the brake relies on and the injected-roll
    //! arithmetic the TS draws from `Math.random()`.

    use super::jittered_retry_after;

    #[test]
    fn zero_jitter_returns_base_unchanged() {
        // The exact `retryAfterJitterSec: 0` brake config: no randomness, the
        // roll closure is never invoked.
        let mut rolled = false;
        let v = jittered_retry_after(2, 0, || {
            rolled = true;
            999
        });
        assert_eq!(v, 2);
        assert!(!rolled, "zero jitter must not consult the roll source");
    }

    #[test]
    fn roll_is_reduced_modulo_jitter_plus_one() {
        // base=10, jitter=4 → offset ∈ [0, 4]; roll=7 ⇒ 7 % 5 = 2 ⇒ 12.
        assert_eq!(jittered_retry_after(10, 4, || 7), 12);
        // roll exactly at a multiple of (jitter+1) ⇒ offset 0 ⇒ base.
        assert_eq!(jittered_retry_after(10, 4, || 5), 10);
        // roll = jitter ⇒ max offset ⇒ base + jitter.
        assert_eq!(jittered_retry_after(10, 4, || 4), 14);
    }

    #[test]
    fn offset_is_bounded_to_zero_through_jitter_inclusive() {
        // Mirrors TS `Math.floor(Math.random() * (jitter + 1))` ∈ [0, jitter]:
        // for every residue r of (jitter+1), base+r stays within [base, base+jitter].
        let (base, jitter) = (30u32, 6u32);
        for roll in 0u64..50 {
            let v = jittered_retry_after(base, jitter, || roll);
            assert!(
                (base..=base + jitter).contains(&v),
                "roll={roll} produced {v}, outside [{base}, {}]",
                base + jitter
            );
        }
    }

    #[test]
    fn large_roll_does_not_overflow() {
        // u64::MAX % (jitter+1) is still a small offset — no panic, in-range.
        let v = jittered_retry_after(1, 9, || u64::MAX);
        assert!((1..=10).contains(&v));
    }
}

#[cfg(test)]
mod build_stateless_reject_503_tests {
    //! Port of the `buildStatelessReject503Buffer` contract from
    //! `src/sip/MessageHelpers.ts` (L267–356). No dedicated TS unit test — it is
    //! exercised end-to-end by the Tier-1 brake (`UdpTransport-brake.test.ts`,
    //! "non-emergency INVITEs past the threshold receive a stateless 503", which
    //! asserts the reply status line starts with `SIP/2.0 503`). These pin the
    //! byte-slicing contract directly: the five mandatory headers copied verbatim
    //! with canonical names, the deliberate absence of a To-tag, the
    //! Reason/Retry-After/Content-Length trailer, the LF-only fallback, compact
    //! header forms, and every `None` rejection branch.

    use super::build_stateless_reject_503_buffer;

    const FLOODER_IP: &str = "10.0.0.1";
    const FLOODER_PORT: u16 = 5555;
    const B2BUA_IP: &str = "127.0.0.1";
    const B2BUA_PORT: u16 = 5060;

    /// The exact non-emergency INVITE the brake's `buildInviteBuffer(i)` emits
    /// (CRLF separators, canonical header names).
    fn invite_buf(i: u32) -> Vec<u8> {
        format!(
            "INVITE sip:bob@{B2BUA_IP}:{B2BUA_PORT} SIP/2.0\r\n\
Via: SIP/2.0/UDP {FLOODER_IP}:{FLOODER_PORT};branch=z9hG4bK-brake-{i}\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag-{i}\r\n\
To: <sip:bob@b2bua.test>\r\n\
Call-ID: brake-test-{i}@{FLOODER_IP}\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@{FLOODER_IP}:{FLOODER_PORT}>\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// Split a built response into its lines (CRLF), as the brake's `statusLine`
    /// helper would read the first one.
    fn resp_lines(buf: &[u8]) -> Vec<String> {
        String::from_utf8(buf.to_vec())
            .unwrap()
            .split("\r\n")
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn templates_a_503_from_a_well_formed_invite() {
        let resp = build_stateless_reject_503_buffer(&invite_buf(0), 5).expect("should template");
        let lines = resp_lines(&resp);

        // Status line is exactly the brake's asserted `SIP/2.0 503 …`.
        assert_eq!(lines[0], "SIP/2.0 503 Service Unavailable");
        assert!(lines[0].starts_with("SIP/2.0 503"));

        // The five required headers, canonical-named, value copied verbatim.
        assert_eq!(
            lines[1],
            "Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-brake-0"
        );
        assert_eq!(lines[2], "From: <sip:alice@flooder.test>;tag=alice-tag-0");
        assert_eq!(lines[3], "To: <sip:bob@b2bua.test>");
        assert_eq!(lines[4], "Call-ID: brake-test-0@10.0.0.1");
        assert_eq!(lines[5], "CSeq: 1 INVITE");

        // Overload trailer.
        assert_eq!(lines[6], "Reason: SIP;cause=503;text=\"overload\"");
        assert_eq!(lines[7], "Retry-After: 5");
        assert_eq!(lines[8], "Content-Length: 0");
        // Header section terminated by a blank line (CRLFCRLF → trailing "", "").
        assert_eq!(lines[9], "");
        assert_eq!(lines[10], "");
    }

    #[test]
    fn does_not_add_a_to_tag() {
        // The cheap-rejection contract: the echoed To carries NO ;tag= (so the
        // UAC's ACK has no dialog context and is dropped as an orphan ACK). The
        // request's To had no tag, and we must not synthesise one.
        let resp = build_stateless_reject_503_buffer(&invite_buf(1), 7).unwrap();
        let to_line = resp_lines(&resp).into_iter().find(|l| l.starts_with("To:")).unwrap();
        assert!(!to_line.contains(";tag="), "Tier-1 503 must not add a To-tag: {to_line:?}");
    }

    #[test]
    fn retry_after_value_is_rendered() {
        let resp = build_stateless_reject_503_buffer(&invite_buf(2), 30).unwrap();
        assert!(resp_lines(&resp).iter().any(|l| l == "Retry-After: 30"));
    }

    #[test]
    fn topmost_via_is_the_one_copied() {
        // Two Via headers (a forwarded request); the response echoes only the
        // first (topmost) — what the UAC matches on.
        let raw = b"INVITE sip:bob@127.0.0.1 SIP/2.0\r\n\
Via: SIP/2.0/UDP top.example:5060;branch=z9hG4bK-top\r\n\
Via: SIP/2.0/UDP bottom.example:5060;branch=z9hG4bK-bot\r\n\
From: <sip:a@x>;tag=ft\r\n\
To: <sip:b@y>\r\n\
Call-ID: cid@x\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
        let resp = build_stateless_reject_503_buffer(raw, 5).unwrap();
        let via = resp_lines(&resp).into_iter().find(|l| l.starts_with("Via:")).unwrap();
        assert_eq!(via, "Via: SIP/2.0/UDP top.example:5060;branch=z9hG4bK-top");
    }

    #[test]
    fn compact_header_forms_are_accepted_and_normalized() {
        // Compact forms v/f/t/i on input → canonical names on output, value
        // copied verbatim (including the original spacing after the colon).
        let raw = b"INVITE sip:bob@127.0.0.1 SIP/2.0\r\n\
v: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-c\r\n\
f: <sip:a@x>;tag=cf\r\n\
t: <sip:b@y>\r\n\
i: compact-cid@x\r\n\
CSeq: 7 INVITE\r\n\
Content-Length: 0\r\n\r\n";
        let resp = build_stateless_reject_503_buffer(raw, 5).unwrap();
        let lines = resp_lines(&resp);
        assert_eq!(lines[1], "Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-c");
        assert_eq!(lines[2], "From: <sip:a@x>;tag=cf");
        assert_eq!(lines[3], "To: <sip:b@y>");
        assert_eq!(lines[4], "Call-ID: compact-cid@x");
        assert_eq!(lines[5], "CSeq: 7 INVITE");
    }

    #[test]
    fn header_name_match_is_case_insensitive() {
        // Mixed-case inbound header names are still matched (RFC 3261 §7.3.1).
        let raw = b"INVITE sip:bob SIP/2.0\r\n\
VIA: SIP/2.0/UDP h:5060;branch=z9hG4bK-u\r\n\
fRoM: <sip:a@x>;tag=u\r\n\
To: <sip:b@y>\r\n\
CALL-ID: u-cid@x\r\n\
cSeQ: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
        let resp = build_stateless_reject_503_buffer(raw, 5).unwrap();
        let lines = resp_lines(&resp);
        assert_eq!(lines[1], "Via: SIP/2.0/UDP h:5060;branch=z9hG4bK-u");
        assert_eq!(lines[2], "From: <sip:a@x>;tag=u");
        assert_eq!(lines[4], "Call-ID: u-cid@x");
    }

    #[test]
    fn first_occurrence_of_each_header_wins() {
        // Duplicate From: only the first is copied (the TS `if (x === undefined)`
        // guard).
        let raw = b"INVITE sip:bob SIP/2.0\r\n\
Via: SIP/2.0/UDP h:5060;branch=z9hG4bK-1\r\n\
From: <sip:first@x>;tag=one\r\n\
From: <sip:second@x>;tag=two\r\n\
To: <sip:b@y>\r\n\
Call-ID: cid@x\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
        let resp = build_stateless_reject_503_buffer(raw, 5).unwrap();
        let from = resp_lines(&resp).into_iter().find(|l| l.starts_with("From:")).unwrap();
        assert_eq!(from, "From: <sip:first@x>;tag=one");
    }

    #[test]
    fn lf_only_separators_are_accepted_and_output_is_crlf() {
        // LFLF fallback (lineSep = "\n"): the request uses bare LF, but the
        // templated reply is always CRLF-terminated.
        let raw = b"INVITE sip:bob SIP/2.0\n\
Via: SIP/2.0/UDP h:5060;branch=z9hG4bK-lf\n\
From: <sip:a@x>;tag=lf\n\
To: <sip:b@y>\n\
Call-ID: lf-cid@x\n\
CSeq: 1 INVITE\n\
Content-Length: 0\n\n";
        let resp = build_stateless_reject_503_buffer(raw, 5).expect("LF-only should template");
        // Output is CRLF regardless of input separator.
        assert!(resp.starts_with(b"SIP/2.0 503 Service Unavailable\r\n"));
        let lines = resp_lines(&resp);
        assert_eq!(lines[1], "Via: SIP/2.0/UDP h:5060;branch=z9hG4bK-lf");
        assert_eq!(lines[2], "From: <sip:a@x>;tag=lf");
    }

    #[test]
    fn folded_continuation_line_is_skipped_not_misparsed() {
        // A folded Via value (continuation line starts with SP) — the
        // continuation is skipped as a header line; the first physical Via line
        // is copied verbatim (the TS copies the single physical line, not the
        // unfolded value).
        let raw = b"INVITE sip:bob SIP/2.0\r\n\
Via: SIP/2.0/UDP h:5060\r\n ;branch=z9hG4bK-folded\r\n\
From: <sip:a@x>;tag=fold\r\n\
To: <sip:b@y>\r\n\
Call-ID: fold-cid@x\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
        let resp = build_stateless_reject_503_buffer(raw, 5).unwrap();
        let lines = resp_lines(&resp);
        // The Via line copied is the first physical line; the continuation did
        // not get mistaken for a new header (no header line is a bare " ;...").
        assert_eq!(lines[1], "Via: SIP/2.0/UDP h:5060");
        // From/To/etc. still found after the continuation.
        assert_eq!(lines[2], "From: <sip:a@x>;tag=fold");
    }

    // ---- None / rejection branches -------------------------------------

    #[test]
    fn no_header_terminator_returns_none() {
        // No CRLFCRLF and no LFLF anywhere → cannot find the header section.
        let raw = b"INVITE sip:bob SIP/2.0\r\nVia: SIP/2.0/UDP h\r\nFrom: x";
        assert!(build_stateless_reject_503_buffer(raw, 5).is_none());
    }

    #[test]
    fn response_first_line_returns_none() {
        // First line lacks a request shape but DOES contain SIP/2.0 — however a
        // genuine *response* still gets templated only if it has the 5 headers;
        // the realistic rejection is a first line with NO "SIP/2.0" token.
        let raw = b"GARBAGE LINE NO VERSION\r\nVia: x\r\n\r\n";
        assert!(build_stateless_reject_503_buffer(raw, 5).is_none());
    }

    #[test]
    fn missing_required_header_returns_none() {
        // Has Via/From/To/Call-ID but NO CSeq → not templatable.
        let raw = b"INVITE sip:bob SIP/2.0\r\n\
Via: SIP/2.0/UDP h:5060;branch=z9hG4bK-n\r\n\
From: <sip:a@x>;tag=n\r\n\
To: <sip:b@y>\r\n\
Call-ID: n-cid@x\r\n\
Content-Length: 0\r\n\r\n";
        assert!(build_stateless_reject_503_buffer(raw, 5).is_none());
    }

    #[test]
    fn header_section_with_only_a_request_line_returns_none() {
        // CRLFCRLF immediately after the request line → lines = [requestLine],
        // fewer than 2 lines → None (the TS `lines.length < 2` guard).
        let raw = b"INVITE sip:bob SIP/2.0\r\n\r\n";
        assert!(build_stateless_reject_503_buffer(raw, 5).is_none());
    }

    #[test]
    fn empty_buffer_returns_none() {
        assert!(build_stateless_reject_503_buffer(b"", 5).is_none());
    }
}

#[cfg(test)]
mod tier1_brake_helper_composition_tests {
    //! The three named cases from `tests/sip/UdpTransport-brake.test.ts`,
    //! re-expressed at the **helper-composition** level — the layer this
    //! migration item owns. The end-to-end brake test drives a full
    //! `UdpTransport` + simulated fabric (deferred with that facade, see the
    //! module doc), but the *decision* each case asserts is exactly the boolean
    //! the future `preIngress` closure computes from these pure helpers:
    //!
    //! ```ignore
    //! if depth >= tier1_threshold
    //!     && is_invite_request_buffer(raw)
    //!     && !buffer_has_emergency_marker(raw)
    //! { reply(build_stateless_reject_503_buffer(raw, jittered_retry_after(base, jitter, roll))) }
    //! else { accept() }
    //! ```
    //!
    //! Replaying that predicate here pins the brake's shed/bypass logic without
    //! the network slice, so the three TS cases are accounted for now and the
    //! facade port later only has to wire the (already-tested) pieces together.

    use super::{
        build_stateless_reject_503_buffer, buffer_has_emergency_marker, is_invite_request_buffer,
        jittered_retry_after,
    };

    const B2BUA_IP: &str = "127.0.0.1";
    const B2BUA_PORT: u16 = 5060;
    const FLOODER_IP: &str = "10.0.0.1";
    const FLOODER_PORT: u16 = 5555;
    // testConfig: udpQueueMax = 5, udpQueueTier1ThresholdPct = 40 →
    // tier1Threshold = floor(5 * 40 / 100) = 2; retryAfterBaseSec default,
    // retryAfterJitterSec = 0.
    const TIER1_THRESHOLD: usize = 2;
    const RETRY_AFTER_BASE: u32 = 5;
    const RETRY_AFTER_JITTER: u32 = 0;

    /// Mirror of the brake's `buildInviteBuffer(i, { emergency })`.
    fn invite_buf(i: u32, emergency: bool) -> Vec<u8> {
        let mut s = format!(
            "INVITE sip:bob@{B2BUA_IP}:{B2BUA_PORT} SIP/2.0\r\n\
Via: SIP/2.0/UDP {FLOODER_IP}:{FLOODER_PORT};branch=z9hG4bK-brake-{i}\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag-{i}\r\n\
To: <sip:bob@b2bua.test>\r\n\
Call-ID: brake-test-{i}@{FLOODER_IP}\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@{FLOODER_IP}:{FLOODER_PORT}>\r\n\
Max-Forwards: 70\r\n"
        );
        if emergency {
            s.push_str("Resource-Priority: esnet.0\r\n");
        }
        s.push_str("Content-Length: 0\r\n\r\n");
        s.into_bytes()
    }

    /// Mirror of the brake's `buildOptionsBuffer(i)`.
    fn options_buf(i: u32) -> Vec<u8> {
        format!(
            "OPTIONS sip:bob@{B2BUA_IP}:{B2BUA_PORT} SIP/2.0\r\n\
Via: SIP/2.0/UDP {FLOODER_IP}:{FLOODER_PORT};branch=z9hG4bK-opts-{i}\r\n\
From: <sip:alice@flooder.test>;tag=opt-{i}\r\n\
To: <sip:bob@b2bua.test>\r\n\
Call-ID: opts-{i}@{FLOODER_IP}\r\n\
CSeq: 1 OPTIONS\r\n\
Max-Forwards: 70\r\n\
Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// The exact predicate the future Tier-1 `preIngress` hook evaluates. Returns
    /// the 503 buffer to reply with, or `None` to accept/enqueue. `depth` is the
    /// current inbound-queue depth the fabric passes the hook.
    fn brake_decision(raw: &[u8], depth: usize) -> Option<Vec<u8>> {
        if depth >= TIER1_THRESHOLD
            && is_invite_request_buffer(raw)
            && !buffer_has_emergency_marker(raw)
        {
            let retry_after = jittered_retry_after(RETRY_AFTER_BASE, RETRY_AFTER_JITTER, || 0);
            // A malformed buffer that fails templating falls through to accept,
            // exactly as the TS hook does (`if (respBuf !== null)`).
            return build_stateless_reject_503_buffer(raw, retry_after);
        }
        None
    }

    #[test]
    fn non_emergency_invites_past_the_threshold_receive_a_stateless_503() {
        // Replays "non-emergency INVITEs past the threshold receive a stateless
        // 503". Flood 10 INVITEs into an undrained queue: depths 0 and 1 are
        // below threshold (accepted); depth 2 and every one after is rejected.
        let flood = 10usize;
        let mut rejects = 0;
        for i in 0..flood {
            // No drain → depth equals the count already accepted.
            let depth = i.min(TIER1_THRESHOLD); // 0,1,2,2,2,... like the live queue
            match brake_decision(&invite_buf(i as u32, false), depth) {
                Some(resp) => {
                    rejects += 1;
                    // The reply the flooder would receive is a 503.
                    assert!(resp.starts_with(b"SIP/2.0 503"));
                }
                None => assert!(i < TIER1_THRESHOLD, "INVITE {i} below threshold must be accepted"),
            }
        }
        // floodCount - 2 rejects, matching `expectedRejects` in the TS.
        assert_eq!(rejects, flood - TIER1_THRESHOLD);
    }

    #[test]
    fn emergency_invites_bypass_the_brake_even_above_the_threshold() {
        // Replays "emergency INVITEs bypass the brake even when above the
        // threshold". Two non-emergency INVITEs fill to threshold, then an
        // emergency INVITE at depth == threshold must NOT be rejected.
        assert!(brake_decision(&invite_buf(0, false), 0).is_none()); // accepted
        assert!(brake_decision(&invite_buf(1, false), 1).is_none()); // accepted
        // At depth 2 (== threshold) a non-emergency INVITE WOULD be shed...
        assert!(brake_decision(&invite_buf(99, false), 2).is_some());
        // ...but the emergency INVITE bypasses via buffer_has_emergency_marker.
        assert!(
            brake_decision(&invite_buf(2, true), 2).is_none(),
            "emergency INVITE must bypass the brake"
        );
    }

    #[test]
    fn non_invite_requests_are_not_503d_by_the_brake() {
        // Replays "non-INVITE requests are not 503'd by the brake". With the
        // queue saturated (depth >= threshold), an OPTIONS is still accepted —
        // the brake only targets new INVITEs.
        // Saturating INVITEs at/above threshold are shed...
        for i in 2..5u32 {
            assert!(brake_decision(&invite_buf(i, false), 2).is_some());
        }
        // ...the OPTIONS at the same saturated depth is NOT (isInviteRequestBuffer
        // is false), so no 503 is templated for it.
        assert!(
            brake_decision(&options_buf(0), 2).is_none(),
            "non-INVITE must not be 503'd"
        );
    }
}
