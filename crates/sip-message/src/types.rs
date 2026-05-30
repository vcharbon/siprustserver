//! Core SIP message types. Port of `src/sip/types.ts` + the typed-registry
//! role of `src/sip/header-registry.ts`.
//!
//! The TS `getHeader<K>` typed registry is replaced by a layered, type-safe
//! model (see docs/adr/0003-typed-header-model.md):
//!
//!   A. Mandatory headers are eager **non-`Option` fields** — the parser is
//!      the gate (it rejects messages missing them), so consumers never check.
//!   B. Mandatory non-empty lists use [`NonEmpty`] — `via.first()` is `&Via`.
//!   C. Grammar-mandatory sub-fields are non-`Option` (a `NameAddr.uri` always
//!      exists). Context-dependent sub-fields (a `tag`) stay `Option` on the
//!      base and become infallible on a refined view.
//!   D. Context guarantees are **flat refined newtype views** built once at a
//!      boundary ([`InDialogRequest`], [`InviteRequest`], …), `Deref`-ing to
//!      the base so no accessor is duplicated. Combined on demand.
//!   E. Extensibility: the [`TypedHeader`] trait (typed, open, compile-time —
//!      the registry replacement) plus the raw [`SipMessage::get_header`]
//!      `Vec<&str>` escape hatch for unknown headers.

use std::collections::BTreeMap;
use std::ops::Deref;

use crate::error::SipParseError;

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SipHeader {
    /// Original case.
    pub name: String,
    /// Trimmed value.
    pub value: String,
}

/// A structured-header parameter: a bare flag (`;lr`) or `;k=v`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamValue {
    Flag,
    Value(String),
}

pub type Params = BTreeMap<String, ParamValue>;

/// A list guaranteed to hold at least one element — the port of the source's
/// `NonEmptyReadonlyArray`. `first()` returns `&T`, never `Option`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmpty<T> {
    head: T,
    tail: Vec<T>,
}

impl<T> NonEmpty<T> {
    pub fn new(head: T) -> Self {
        Self { head, tail: Vec::new() }
    }
    pub fn from_parts(head: T, tail: Vec<T>) -> Self {
        Self { head, tail }
    }
    /// The first element — for Via, the top (response-routing) hop.
    pub fn first(&self) -> &T {
        &self.head
    }
    pub fn len(&self) -> usize {
        1 + self.tail.len()
    }
    pub fn is_empty(&self) -> bool {
        false
    }
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        std::iter::once(&self.head).chain(self.tail.iter())
    }
}

// ---------------------------------------------------------------------------
// Parsed structured fields
// ---------------------------------------------------------------------------

/// name-addr / addr-spec. `uri` is always present (grammar-mandatory); `tag`
/// is context-dependent — guaranteed only on a refined view (see [`InDialogRequest`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameAddr {
    pub display_name: Option<String>,
    pub uri: String,
    pub tag: Option<String>,
    pub params: Params,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Via {
    pub transport: String,
    pub host: String,
    pub port: Option<u16>,
    pub branch: Option<String>,
    pub params: Params,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    pub display_name: Option<String>,
    pub uri: String,
    pub params: Params,
}

/// Every Contact on a message. `Contact: *` (RFC 3261 §10.2.2 wildcard) is a
/// distinct variant — a bare token, not a URI, that must stand alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContactSet {
    Wildcard,
    Contacts(Vec<Contact>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSeq {
    pub seq: u32,
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestUri {
    pub scheme: String,
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    pub params: BTreeMap<String, String>,
}

/// A parsed SIP URI as it appears in an optional header (e.g. Refer-To). Port
/// of the structured parser's `ParsedUri`. Unlike [`RequestUri`], the port is
/// not range-validated here (kept as the raw parsed value), so it is `u64`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uri {
    pub scheme: String,
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u64>,
    pub params: BTreeMap<String, String>,
}

/// RFC 3262 RAck value: `response-num CSeq-num method`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rack {
    pub rseq: u64,
    pub seq: u64,
    pub method: String,
}

/// RFC 3891 Replaces value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replaces {
    pub call_id: String,
    pub to_tag: String,
    pub from_tag: String,
    pub early_only: bool,
}

/// RFC 3515 Refer-To value (with RFC 3891 embedded Replaces).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferTo {
    pub display_name: Option<String>,
    pub uri: String,
    pub parsed_uri: Option<Uri>,
    pub params: Params,
    pub embedded_headers: BTreeMap<String, String>,
    pub replaces: Option<Replaces>,
}

/// Optional structured headers, parsed **eagerly + non-fatally** (per
/// docs/adr/0003): each is a `Result` so a malformed optional header does NOT
/// reject the message (pass-through tolerance), but the error is captured and
/// surfaced on access — and by [`SipMessage::validate_strict`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionalHeaders {
    pub p_asserted_identity: Result<Vec<NameAddr>, crate::error::SipParseError>,
    pub p_preferred_identity: Result<Vec<NameAddr>, crate::error::SipParseError>,
    pub diversion: Result<Vec<NameAddr>, crate::error::SipParseError>,
    pub history_info: Result<Vec<NameAddr>, crate::error::SipParseError>,
    pub remote_party_id: Result<Vec<NameAddr>, crate::error::SipParseError>,
    pub geolocation: Result<Vec<NameAddr>, crate::error::SipParseError>,
    pub geolocation_error: Result<Vec<NameAddr>, crate::error::SipParseError>,
    pub geolocation_routing: Result<Option<bool>, crate::error::SipParseError>,
    pub rack: Result<Option<Rack>, crate::error::SipParseError>,
    pub refer_to: Result<Option<ReferTo>, crate::error::SipParseError>,
}

// ---------------------------------------------------------------------------
// Messages — mandatory headers as eager non-Option fields (Mechanism A/B/C)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SipRequest {
    pub method: String,
    /// Request-URI (raw string, on the wire).
    pub uri: String,
    pub request_uri: RequestUri,
    pub version: String,
    // Eager mandatory fields — parser rejects the message if any is missing.
    pub from: NameAddr,
    pub to: NameAddr,
    pub call_id: String,
    pub cseq: CSeq,
    pub via: NonEmpty<Via>,
    pub contacts: ContactSet,
    /// Optional structured headers, eagerly + non-fatally parsed.
    pub optional: OptionalHeaders,
    /// Full header list in wire order — for raw access + faithful serialization.
    pub headers: Vec<SipHeader>,
    /// Raw body bytes — opaque to the B2BUA.
    pub body: Vec<u8>,
    /// Original packet bytes.
    pub raw: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SipResponse {
    pub version: String,
    pub status: u16,
    pub reason: String,
    pub from: NameAddr,
    /// `to.tag` is absent on `100 Trying`, present otherwise — see [`SipResponseTagged`].
    pub to: NameAddr,
    pub call_id: String,
    pub cseq: CSeq,
    pub via: NonEmpty<Via>,
    /// Contacts carried on the response — multiple on a 3xx redirect (the
    /// redirect targets, RFC 3261 §21.4) or the single UA contact on a 2xx.
    /// Validated at parse time like the request set.
    pub contacts: ContactSet,
    /// Optional structured headers, eagerly + non-fatally parsed.
    pub optional: OptionalHeaders,
    pub headers: Vec<SipHeader>,
    pub body: Vec<u8>,
    pub raw: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SipMessage {
    Request(SipRequest),
    Response(SipResponse),
}

impl SipMessage {
    pub fn headers(&self) -> &[SipHeader] {
        match self {
            SipMessage::Request(r) => &r.headers,
            SipMessage::Response(r) => &r.headers,
        }
    }

    /// Raw escape hatch (Mechanism E). Case-insensitive, wire order preserved.
    /// For unknown/extension headers; built-ins have typed accessors above.
    pub fn get_header(&self, name: &str) -> Vec<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers()
            .iter()
            .filter(|h| h.name.to_ascii_lowercase() == lower)
            .map(|h| h.value.as_str())
            .collect()
    }

    pub fn has_header(&self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        self.headers().iter().any(|h| h.name.to_ascii_lowercase() == lower)
    }

    /// The eagerly-parsed optional structured headers.
    pub fn optional(&self) -> &OptionalHeaders {
        match self {
            SipMessage::Request(r) => &r.optional,
            SipMessage::Response(r) => &r.optional,
        }
    }

    /// Strict header-content validation — the port of `runAllStrictLazyParsers`.
    /// `parse()` is tolerant (a malformed optional header does not reject the
    /// message); this opt-in pass re-validates Date/From/To/Contact grammar and
    /// every optional structured header, returning the first violation. Used by
    /// the compliance matrix and any security-sensitive caller.
    pub fn validate_strict(&self) -> Result<(), crate::error::SipParseError> {
        crate::parser::custom::optional_headers::run_all_strict(self.headers())
    }
}

// ---------------------------------------------------------------------------
// Typed extension registry (Mechanism E) — replaces declaration-merging +
// SipHeaderRegistry.register. Open, compile-time, no global mutable state.
// ---------------------------------------------------------------------------

/// A typed custom/extension header. Integrators implement this in their own
/// crate; `msg.typed::<MyHeader>()` then returns the parsed value with no
/// change to this core. Built-ins are NOT `TypedHeader` impls (they are
/// concrete fields/methods), so there is no collision risk.
///
/// NOTE: unlike the TS registry, `typed` does not memoize — it re-parses on
/// call (Rust parsing is cheap; a per-message type-erased cache fights the
/// borrow checker). Bind to a `let` on hot paths. Tracked perf knob.
pub trait TypedHeader: Sized {
    /// Canonical lowercase long-form name (e.g. "x-routing-hint").
    const NAME: &'static str;
    /// Optional alias names (e.g. compact forms).
    const ALIASES: &'static [&'static str] = &[];
    fn parse(raw_values: &[&str]) -> Result<Self, SipParseError>;
}

impl SipMessage {
    /// Typed extension access — the registry replacement.
    pub fn typed<H: TypedHeader>(&self) -> Result<H, SipParseError> {
        H::parse(&self.get_header(H::NAME))
    }
}

// ---------------------------------------------------------------------------
// Refined views (Mechanism D) — flat, borrowed, Deref to the base. Built once
// at a boundary (router/dispatch); downstream code is never defensive.
// Combined on demand (see docs/adr/0003 — Rust has no intersection types).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotInDialog;

/// A request the router has confirmed in-dialog: both From-tag and To-tag are
/// present. `from_tag()` / `to_tag()` are infallible `&str`.
#[derive(Debug, Clone, Copy)]
pub struct InDialogRequest<'a>(&'a SipRequest);

impl<'a> InDialogRequest<'a> {
    /// The single validation choke point — the only constructor.
    pub fn new(r: &'a SipRequest) -> Result<Self, NotInDialog> {
        if r.from.tag.is_some() && r.to.tag.is_some() {
            Ok(Self(r))
        } else {
            Err(NotInDialog)
        }
    }
    /// Sound by construction — `new` checked presence.
    pub fn from_tag(&self) -> &str {
        self.0.from.tag.as_deref().expect("InDialogRequest invariant: From-tag present")
    }
    pub fn to_tag(&self) -> &str {
        self.0.to.tag.as_deref().expect("InDialogRequest invariant: To-tag present")
    }
}

impl<'a> Deref for InDialogRequest<'a> {
    type Target = SipRequest;
    fn deref(&self) -> &SipRequest {
        self.0
    }
}

/// An INVITE request. An INVITE carries at most one Contact (the UA's), so
/// `contact()` is a single `Option<&Contact>` — distinct from the base
/// `contacts` set valid on REGISTER / 3xx.
#[derive(Debug, Clone, Copy)]
pub struct InviteRequest<'a>(&'a SipRequest);

impl<'a> InviteRequest<'a> {
    pub fn new(r: &'a SipRequest) -> Option<Self> {
        (r.method == "INVITE").then_some(Self(r))
    }
    /// The single Contact, if present.
    pub fn contact(&self) -> Option<&Contact> {
        match &self.0.contacts {
            ContactSet::Contacts(cs) => cs.first(),
            ContactSet::Wildcard => None,
        }
    }
}

impl<'a> Deref for InviteRequest<'a> {
    type Target = SipRequest;
    fn deref(&self) -> &SipRequest {
        self.0
    }
}

/// A response with a guaranteed To-tag (every non-`100` response). `to_tag()`
/// is infallible.
#[derive(Debug, Clone, Copy)]
pub struct SipResponseTagged<'a>(&'a SipResponse);

impl<'a> SipResponseTagged<'a> {
    pub fn new(r: &'a SipResponse) -> Result<Self, NotInDialog> {
        if r.to.tag.is_some() {
            Ok(Self(r))
        } else {
            Err(NotInDialog)
        }
    }
    pub fn to_tag(&self) -> &str {
        self.0.to.tag.as_deref().expect("SipResponseTagged invariant: To-tag present")
    }
}

impl<'a> Deref for SipResponseTagged<'a> {
    type Target = SipResponse;
    fn deref(&self) -> &SipResponse {
        self.0
    }
}
