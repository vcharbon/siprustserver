//! `MessageTemplate` — a captured SIP message prepared for near-verbatim
//! replay. The stack regenerates the dialog-critical (tier-1) fields from live
//! dialog/transaction state and emits every other header byte-for-byte.
//!
//! A template splits its headers by the crate's one ownership table,
//! [`HeaderClass`]:
//!
//! - **Structural** — the fields the stack MUST own for the replayed message to
//!   be a valid, routable member of a live dialog: `Via` (+branch), `From`/`To`
//!   (tags), `Call-ID`, `CSeq` (number), `Max-Forwards`, `Content-Length`,
//!   `Contact` (host:port) and the `Route`/`Record-Route` set. Their stored
//!   values are dropped on emission; the generator fills them.
//! - **EndToEnd** — every other header, emitted byte-for-byte: value bytes,
//!   header-name casing, and duplicate-header layout all preserved.
//!   `Content-Type` is deliberately end-to-end (a captured unusual media type
//!   must survive), even though `Content-Length` is structural.
//!
//! # Fidelity is relative to the PARSED value
//!
//! "Byte-for-byte" preservation is of the *parsed* header value, not the raw
//! capture bytes: a folded value (RFC 3261 §7.3.1 `\r\n`+WSP line continuation)
//! is emitted unfolded, and surrounding optional whitespace is trimmed — exactly
//! the normalization [`SipHeader`] already applies. This is stable across the
//! extract → replay → re-extract loop but is not the original wire framing.
//!
//! # Compact header-name fidelity, and the ONE canonicalization source
//!
//! A message captured with RFC 3261 §7.3.3 compact names (`v` Via, `f` From,
//! `t` To, `i` Call-ID, `m` Contact, `c` Content-Type, …) replays with those
//! compact names — for frozen headers AND for the tier-1 headers the stack
//! regenerates. The parser canonicalizes `SipHeader.name` (so all matching stays
//! canonical), and [`from_message`](MessageTemplate::from_message) recovers the
//! captured wire form from the exact bytes. Templates from captures MUST be built
//! via `from_message` over wire bytes; the raw-line constructors are for tests.
//! Header-name classification / canonicalization has ONE source of truth — the
//! parser's compact-form table (`HeaderName::class_of` and the presence probes both
//! expand through it). v1 boundary: a REGENERATED header captured in mixed
//! name-forms across duplicate rows (`Via:` + `v:`) replays under one compact
//! form — see [`MessageTemplate::regenerated_name_forms`].
//!
//! # Automatics are not template-overridable in v1
//!
//! Stack automatics and dedicated primitives — `100 Trying`, the `ACK` to a
//! final, `CANCEL`, and the `487` a CANCEL triggers — cannot carry template
//! headers in v1: they are emitted by the stack / their own primitives, which
//! take no template. A captured ACK's or CANCEL's frozen-header quirks are not
//! replayable yet.
//!
//! # v1 emission limitation
//!
//! Header ORDER is canonical in v1: the stack emits its regenerated block first,
//! then the frozen headers in their captured relative order. [`EmitOpts`] carries
//! `preserve_order` so a caller can request original absolute ordering, but v1
//! honours it only for casing + duplicate-header layout (always preserved for
//! frozen headers regardless) — it does NOT yet restore the original interleaving
//! of frozen and regenerated headers. The flag is threaded for API stability.

pub use crate::header::HeaderClass;
use crate::header::HeaderName;
use crate::method::Method;
use crate::types::{SipHeader, SipMessage};

/// One header of a [`MessageTemplate`]: its **canonical** name (compact forms
/// expanded, casing preserved), its value, how the stack treats it on emission,
/// and the as-wire name-form to emit under when the capture used a compact form
/// (`wire_name = Some("v")` for a `v:` Via; `None` when the wire name already
/// equals the canonical name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateHeader {
    pub name: String,
    pub value: String,
    pub class: HeaderClass,
    /// The captured wire name-form when it differs from `name` (a §7.3.3 compact
    /// form). `None` for the raw-line constructors and for headers captured under
    /// their canonical name. Attribution assumes the message's `headers` and
    /// `raw` are the parse-time originals (the parse-then-template contract): a
    /// post-parse header mutation could misalign this against the raw bytes.
    pub wire_name: Option<String>,
}

impl TemplateHeader {
    /// A header classified by its name (the default tier model). No wire-form
    /// override — for the raw-line (test) constructors; captures set the wire
    /// form via [`MessageTemplate::from_message`].
    pub fn classified(name: impl Into<String>, value: impl Into<String>) -> Self {
        let name = name.into();
        let class = HeaderName::class_of(&name);
        TemplateHeader { name, value: value.into(), class, wire_name: None }
    }

    /// A header forced [`Frozen`](HeaderClass::EndToEnd) regardless of name.
    pub fn frozen(name: impl Into<String>, value: impl Into<String>) -> Self {
        TemplateHeader {
            name: name.into(),
            value: value.into(),
            class: HeaderClass::EndToEnd,
            wire_name: None,
        }
    }

    /// A header forced [`Regenerated`](HeaderClass::Structural) regardless of name.
    pub fn regenerated(name: impl Into<String>, value: impl Into<String>) -> Self {
        TemplateHeader {
            name: name.into(),
            value: value.into(),
            class: HeaderClass::Structural,
            wire_name: None,
        }
    }

    /// The name-form to EMIT under: the captured wire form if present, else the
    /// canonical name.
    fn emit_name(&self) -> &str {
        self.wire_name.as_deref().unwrap_or(&self.name)
    }
}

/// The start line a [`MessageTemplate`] reproduces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateStart {
    /// A request of this method (the Request-URI is a tier-1 regenerated field,
    /// so it is not stored on the template).
    Request(Method),
    /// A response of this status code / reason phrase.
    Response { status: u16, reason: String },
}

/// A captured SIP message prepared for near-verbatim replay. Holds the start
/// line, an ordered list of classified headers (wire casing + duplicate layout
/// preserved), and the raw body bytes. A template describes a message a UA
/// deliberately emits; the stack automatics and dedicated primitives (`100`,
/// `ACK`, `CANCEL`, `487`) take no template in v1 (module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageTemplate {
    start: TemplateStart,
    headers: Vec<TemplateHeader>,
    body: Vec<u8>,
}

impl MessageTemplate {
    /// A request template with explicitly classified headers.
    pub fn request(method: Method, headers: Vec<TemplateHeader>, body: Vec<u8>) -> Self {
        MessageTemplate { start: TemplateStart::Request(method), headers, body }
    }

    /// A response template with explicitly classified headers.
    pub fn response(
        status: u16,
        reason: impl Into<String>,
        headers: Vec<TemplateHeader>,
        body: Vec<u8>,
    ) -> Self {
        MessageTemplate {
            start: TemplateStart::Response { status, reason: reason.into() },
            headers,
            body,
        }
    }

    /// Build a template from a parsed message, classifying every header by
    /// (canonical) name and recovering each header's captured **wire** name-form
    /// from the message's original bytes — so a §7.3.3 compact name (`v`, `f`,
    /// `t`, `i`, `m`, `c`, …) replays compact. The header list keeps the message's
    /// wire order, casing, and duplicate-header layout.
    ///
    /// This is THE capture path: it reads the exact wire bytes (`SipRequest.raw`
    /// / `SipResponse.raw`). A message with no retained raw bytes (one the stack
    /// generated rather than parsed) yields canonical names only. The raw-line
    /// [`request`](Self::request) / [`response`](Self::response) constructors are
    /// for tests.
    ///
    /// Scans under the default parser limits. If the message was parsed under
    /// laxer limits, the re-scan may fail and every header falls back to its
    /// canonical name (compact fidelity lost, not incorrect); use
    /// [`from_message_with_limits`](Self::from_message_with_limits) with the
    /// original limits to avoid that.
    pub fn from_message(msg: &SipMessage) -> Self {
        Self::from_message_with_limits(msg, &crate::parser::SipParserLimits::default())
    }

    /// [`from_message`](Self::from_message) scanning the wire name-forms under an
    /// explicit parser-limits set — pass the limits the message was PARSED under
    /// so the re-scan of its raw bytes reproduces the same header split. A
    /// mismatch (scanning stricter than the message was parsed) makes the scan
    /// fail and every header fall back to its canonical name (no compact
    /// fidelity, never wrong values).
    pub fn from_message_with_limits(
        msg: &SipMessage,
        limits: &crate::parser::SipParserLimits,
    ) -> Self {
        let (start, body, raw) = match msg {
            SipMessage::Request(r) => {
                (TemplateStart::Request(r.method().clone()), r.body().clone(), r.image())
            }
            SipMessage::Response(r) => (
                TemplateStart::Response { status: r.status(), reason: r.reason().to_string() },
                r.body().clone(),
                r.image(),
            ),
        };
        // Recover per-header wire names from the exact capture bytes; align 1:1
        // with the parsed header list, else fall back to canonical (no compact).
        let wire_names = if raw.is_empty() {
            Vec::new()
        } else {
            crate::parser::custom::headers::scan_header_name_forms(raw, limits)
        };
        let parsed = msg.headers();
        let aligned = wire_names.len() == parsed.len();
        let headers = parsed
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let wire_name = if aligned && wire_names[i] != h.name {
                    Some(wire_names[i].clone())
                } else {
                    None
                };
                TemplateHeader {
                    name: h.name.to_string(),
                    value: h.value.to_string(),
                    class: HeaderName::class_of(&h.name),
                    wire_name,
                }
            })
            .collect();
        MessageTemplate { start, headers, body: body.to_vec() }
    }

    /// The start line this template reproduces.
    pub fn start(&self) -> &TemplateStart {
        &self.start
    }

    /// The request method, if this is a request template.
    pub fn method(&self) -> Option<&Method> {
        match &self.start {
            TemplateStart::Request(m) => Some(m),
            TemplateStart::Response { .. } => None,
        }
    }

    /// The `(status, reason)`, if this is a response template.
    pub fn status(&self) -> Option<(u16, &str)> {
        match &self.start {
            TemplateStart::Response { status, reason } => Some((*status, reason.as_str())),
            TemplateStart::Request(_) => None,
        }
    }

    /// All classified headers in captured order.
    pub fn headers(&self) -> &[TemplateHeader] {
        &self.headers
    }

    /// The frozen headers as ready-to-emit [`SipHeader`]s, in captured order,
    /// casing, duplicate layout, and **wire name-form** preserved (a compact
    /// `c:` Content-Type emits `c:`) — the `extra_headers` a stack generator
    /// appends verbatim after its regenerated block.
    pub fn frozen_headers(&self) -> Vec<SipHeader> {
        self.headers
            .iter()
            .filter(|h| h.class == HeaderClass::EndToEnd)
            .map(|h| SipHeader::new(h.emit_name(), h.value.as_str()))
            .collect()
    }

    /// The captured compact name-forms of the REGENERATED (tier-1) headers the
    /// stack itself emits — `(canonical, wire)` pairs such as `("Via", "v")`,
    /// `("From", "f")`, `("To", "t")`, `("Call-ID", "i")`, `("Contact", "m")`.
    /// The emission path applies these (via [`apply_name_forms`]) so a capture
    /// that used compact names replays compact on the stack-owned lines too.
    ///
    /// `Content-Length` is deliberately excluded: its name-form is serializer-
    /// owned (the serializer enforces the value by the canonical name), so it
    /// always emits under the canonical name.
    ///
    /// v1 boundary — mixed-form duplicate rows collapse: the map is keyed by
    /// canonical name and [`apply_name_forms`] rewrites EVERY row of a matching
    /// header, so a regenerated header captured in mixed forms across duplicate
    /// rows (`Via:` + `v:`) replays entirely under the one compact form. This is
    /// intrinsic to regenerating these headers (the stack emits its own count,
    /// not the capture's rows) and is invisible to the parsed-value repro differ
    /// (which compares canonicalized values). Per-row name-forms are deferred.
    pub fn regenerated_name_forms(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .filter(|h| h.class == HeaderClass::Structural)
            .filter_map(|h| h.wire_name.as_ref().map(|w| (h.name.clone(), w.clone())))
            .filter(|(canon, _)| !canon.eq_ignore_ascii_case("content-length"))
            .collect()
    }

    /// The Regenerated remote-target headers (`Contact`/`Route`/`Record-Route`)
    /// whose host:port is stack-owned (routing-critical) but whose user part +
    /// ALL parameters are captured — `(canonical_lowercase_name, captured_value)`.
    /// The emission path rewrites the generated header's host:port onto the
    /// captured value (via [`apply_remote_target_emits`]), so a param'd/user'd
    /// Contact round-trips. A remote-target header explicitly FROZEN rides
    /// [`frozen_headers`](Self::frozen_headers) verbatim instead (host:port kept).
    pub fn remote_target_emits(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .filter(|h| {
                h.class == HeaderClass::Structural
                    && crate::remote_target::is_remote_target(&h.name)
            })
            .map(|h| (crate::remote_target::canonical(&h.name), h.value.clone()))
            .collect()
    }

    /// The raw body bytes (opaque — SDP, multipart, or any captured payload).
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

/// Options controlling template emission.
///
/// `preserve_order` requests that the original absolute header ORDER be
/// reproduced. See the module doc: v1 honours it only for casing +
/// duplicate-header layout (always preserved for frozen headers), and does NOT
/// yet restore the original interleaving of frozen and regenerated headers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EmitOpts {
    pub preserve_order: bool,
}

/// Rewrite each header whose canonical name matches a `(canonical, wire)` entry
/// to the mapped wire name-form (compact-name replay of the stack-regenerated
/// headers). Non-matching headers pass through unchanged; an empty map is the
/// identity (full names). Values are untouched. Used by the template
/// emission path to write `v:`/`f:`/… for a capture that used compact names.
/// Render `msg` under the header block the template lane states for it — the
/// captured wire names ([`apply_name_forms`]), the remote-target host:port
/// rewrite ([`apply_remote_target_emits`]) and any §7.3.1 fold the replayed UA
/// applies. The start line and body are the message's.
///
/// The result is wire BYTES, never a message: a re-spelled block is a
/// presentation of the message, not a different one, so nothing downstream can
/// mistake it for the canonical value the §17.1.1.3 ACK and the §9.1 CANCEL are
/// built from.
pub fn emitted_wire(msg: &SipMessage, headers: &[SipHeader]) -> Vec<u8> {
    match msg {
        SipMessage::Request(r) => crate::serializer::render(headers, r.body(), |out| {
            crate::serializer::write_request_line(out, r)
        }),
        SipMessage::Response(r) => crate::serializer::render(headers, r.body(), |out| {
            crate::serializer::write_status_line(out, r)
        }),
    }
}

pub fn apply_name_forms(headers: &[SipHeader], forms: &[(String, String)]) -> Vec<SipHeader> {
    if forms.is_empty() {
        return headers.to_vec();
    }
    headers
        .iter()
        .map(|h| {
            let wire = forms
                .iter()
                .find(|(canon, _)| h.name.eq_ignore_ascii_case(canon))
                .map(|(_, w)| w.clone());
            SipHeader::new(wire.unwrap_or_else(|| h.name.to_string()), h.value.clone())
        })
        .collect()
}

/// Rewrite each generated remote-target header to carry the CAPTURED user part +
/// parameters while keeping the generated (bound-socket) host:port. `emits` are
/// the `(canonical, captured_value)` pairs from
/// [`MessageTemplate::remote_target_emits`]; an empty list is the identity. Only
/// the first generated header of each name is rewritten (Contact is single; a
/// Route the stack did not generate is left to the dialog route set).
pub fn apply_remote_target_emits(
    headers: &[SipHeader],
    emits: &[(String, String)],
) -> Vec<SipHeader> {
    if emits.is_empty() {
        return headers.to_vec();
    }
    let mut out = headers.to_vec();
    for (canon, captured) in emits {
        if let Some(h) = out.iter_mut().find(|h| crate::remote_target::canonical(&h.name) == *canon)
        {
            if let Some((host, port)) = crate::remote_target::first_hostport(&h.value) {
                h.value = crate::remote_target::rewrite_hostport(captured, &host, port).into();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
