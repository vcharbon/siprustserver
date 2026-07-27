//! The message types of ADR-0025: an immutable, image-backed message with one
//! source of truth for its headers.
//!
//! A message is frozen. Its fields are private, so the typed core and the
//! header list cannot desync — the only way to a modified message is
//! [`thaw`](SipRequest::thaw) → edit → `freeze`. The read surface lives in
//! [`crate::access`]; this module holds the shapes.
//!
//! What requests and responses share is written once in [`MessageCore`]:
//! the ordered header list, the typed [`CoreHeaders`] the parser validated, the
//! body, and the datagram image every span points into. Only the start line and
//! the per-direction cardinality rules live on the outer types.
//!
//! The refined views of ADR-0003 are unchanged in spirit: [`InDialogRequest`],
//! [`InviteRequest`] and [`SipResponseTagged`] are flat newtypes built once at a
//! boundary, `Deref`-ing to the base so no accessor is duplicated.

use std::ops::Deref;

use bytes::Bytes;

use crate::draft::{RequestLine, StatusLine};
use crate::header;
use crate::method::Method;
use crate::sip_str::SipStr;

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

/// One header line. Both halves are [`SipStr`], so a parsed header points into
/// the message image instead of owning a copy of its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SipHeader {
    /// Original case.
    pub name: SipStr,
    /// Trimmed value.
    pub value: SipStr,
}

impl SipHeader {
    /// Build a header from anything string-like — the construction path for
    /// generated (as opposed to parsed) headers.
    pub fn new(name: impl Into<SipStr>, value: impl Into<SipStr>) -> Self {
        Self { name: name.into(), value: value.into() }
    }
}

/// A list guaranteed to hold at least one element. `first()` returns `&T`,
/// never `Option`.
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

/// Every Contact on a message. `Contact: *` (RFC 3261 §10.2.2 wildcard) is a
/// distinct variant — a bare token, not a URI, that must stand alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContactSet {
    Wildcard,
    Contacts(Vec<header::Contact>),
}

impl ContactSet {
    /// The contacts, or an empty slice for the wildcard.
    pub fn as_slice(&self) -> &[header::Contact] {
        match self {
            ContactSet::Wildcard => &[],
            ContactSet::Contacts(cs) => cs,
        }
    }

    pub fn is_wildcard(&self) -> bool {
        matches!(self, ContactSet::Wildcard)
    }
}

/// Optional structured headers, parsed **eagerly + non-fatally** (ADR-0003):
/// each is a `Result` so a malformed optional header does NOT reject the
/// message, but the error is captured and surfaced on access — and by
/// [`SipMessage::validate_strict`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionalHeaders {
    pub p_asserted_identity: Result<Vec<header::NameAddr>, crate::error::SipParseError>,
    pub p_preferred_identity: Result<Vec<header::NameAddr>, crate::error::SipParseError>,
    pub diversion: Result<Vec<header::NameAddr>, crate::error::SipParseError>,
    pub history_info: Result<Vec<header::NameAddr>, crate::error::SipParseError>,
    pub remote_party_id: Result<Vec<header::NameAddr>, crate::error::SipParseError>,
    pub geolocation: Result<Vec<header::NameAddr>, crate::error::SipParseError>,
    pub geolocation_error: Result<Vec<header::NameAddr>, crate::error::SipParseError>,
    pub geolocation_routing: Result<Option<bool>, crate::error::SipParseError>,
    pub rack: Result<Option<header::RAck>, crate::error::SipParseError>,
    pub refer_to: Result<Option<header::ReferTo>, crate::error::SipParseError>,
}

// ---------------------------------------------------------------------------
// The shared core
// ---------------------------------------------------------------------------

/// The mandatory headers, typed and validated by the parser — so every reader
/// gets them infallibly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreHeaders {
    pub(crate) from: header::From,
    pub(crate) to: header::To,
    pub(crate) call_id: header::CallId,
    pub(crate) cseq: header::CSeq,
    pub(crate) via: NonEmpty<header::Via>,
    pub(crate) contacts: ContactSet,
}

impl CoreHeaders {
    pub(crate) fn new(
        from: header::From,
        to: header::To,
        call_id: header::CallId,
        cseq: header::CSeq,
        via: NonEmpty<header::Via>,
        contacts: ContactSet,
    ) -> Self {
        Self { from, to, call_id, cseq, via, contacts }
    }

    pub fn from(&self) -> &header::From {
        &self.from
    }

    pub fn to(&self) -> &header::To {
        &self.to
    }

    pub fn call_id(&self) -> &header::CallId {
        &self.call_id
    }

    pub fn cseq(&self) -> &header::CSeq {
        &self.cseq
    }

    pub fn via(&self) -> &NonEmpty<header::Via> {
        &self.via
    }

    pub fn contacts(&self) -> &ContactSet {
        &self.contacts
    }
}

/// Everything a request and a response share, written once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageCore {
    /// Full header list in wire order — THE source of truth.
    pub(crate) headers: Vec<SipHeader>,
    pub(crate) core: CoreHeaders,
    pub(crate) optional: OptionalHeaders,
    /// Raw body bytes — opaque to the B2BUA. A slice of the image on a parsed
    /// message, so it costs a refcount, not a copy.
    pub(crate) body: Bytes,
    /// The datagram this message is, whether it was received or built.
    pub(crate) image: Bytes,
}

impl MessageCore {
    pub(crate) fn new(
        headers: Vec<SipHeader>,
        core: CoreHeaders,
        optional: OptionalHeaders,
        body: Bytes,
        image: Bytes,
    ) -> Self {
        Self { headers, core, optional, body, image }
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SipRequest {
    pub(crate) start: RequestLine,
    pub(crate) inner: MessageCore,
}

impl SipRequest {
    pub(crate) fn new(start: RequestLine, inner: MessageCore) -> Self {
        Self { start, inner }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SipResponse {
    pub(crate) start: StatusLine,
    pub(crate) inner: MessageCore,
}

impl SipResponse {
    pub(crate) fn new(start: StatusLine, inner: MessageCore) -> Self {
        Self { start, inner }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SipMessage {
    Request(SipRequest),
    Response(SipResponse),
}

impl SipMessage {
    pub(crate) fn core(&self) -> &MessageCore {
        match self {
            SipMessage::Request(r) => &r.inner,
            SipMessage::Response(r) => &r.inner,
        }
    }

    pub fn headers(&self) -> &[SipHeader] {
        &self.core().headers
    }

    pub fn body(&self) -> &Bytes {
        &self.core().body
    }

    /// The datagram this message is.
    pub fn image(&self) -> &Bytes {
        &self.core().image
    }

    /// The eagerly-parsed optional structured headers.
    pub fn optional(&self) -> &OptionalHeaders {
        &self.core().optional
    }

    /// Strict header-content validation. `parse()` is tolerant (a malformed
    /// optional header does not reject the message); this opt-in pass
    /// re-validates Date/From/To/Contact grammar and every optional structured
    /// header, returning the first violation.
    pub fn validate_strict(&self) -> Result<(), crate::error::SipParseError> {
        crate::parser::custom::optional_headers::run_all_strict(self.headers())
    }
}

// ---------------------------------------------------------------------------
// Refined views — flat, borrowed, Deref to the base. Built once at a boundary;
// downstream code is never defensive.
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
        if r.from().tag().is_some() && r.to().tag().is_some() {
            Ok(Self(r))
        } else {
            Err(NotInDialog)
        }
    }
    /// Sound by construction — `new` checked presence.
    pub fn from_tag(&self) -> &str {
        self.0.from().tag().expect("InDialogRequest invariant: From-tag present")
    }
    pub fn to_tag(&self) -> &str {
        self.0.to().tag().expect("InDialogRequest invariant: To-tag present")
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
/// contact set valid on REGISTER / 3xx.
#[derive(Debug, Clone, Copy)]
pub struct InviteRequest<'a>(&'a SipRequest);

impl<'a> InviteRequest<'a> {
    pub fn new(r: &'a SipRequest) -> Option<Self> {
        (*r.method() == Method::Invite).then_some(Self(r))
    }
    /// The single Contact, if present.
    pub fn contact(&self) -> Option<&header::Contact> {
        self.0.contacts().as_slice().first()
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
        if r.to().tag().is_some() {
            Ok(Self(r))
        } else {
            Err(NotInDialog)
        }
    }
    pub fn to_tag(&self) -> &str {
        self.0.to().tag().expect("SipResponseTagged invariant: To-tag present")
    }
}

impl<'a> Deref for SipResponseTagged<'a> {
    type Target = SipResponse;
    fn deref(&self) -> &SipResponse {
        self.0
    }
}
