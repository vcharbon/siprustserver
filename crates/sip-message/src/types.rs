//! The message types of ADR-0025: an immutable, image-backed message with one
//! source of truth for its headers.
//!
//! A message is frozen. Its fields are private, so the typed core and the
//! header list cannot desync — the only way to a modified message is
//! [`thaw`](SipRequest::thaw) → edit → `freeze`. The read surface lives in
//! `crate::access`; this module holds the shapes.
//!
//! What requests and responses share is written once in [`MessageCore`]:
//! the ordered header list, the typed [`CoreHeaders`] the parser validated, the
//! body, and the datagram image every span points into. Only the start line and
//! the per-direction cardinality rules live on the outer types.
//!
//! The refined views of ADR-0003 are unchanged in spirit: [`InDialogRequest`],
//! [`InviteRequest`] and [`SipResponseTagged`] are flat newtypes built once at a
//! boundary, `Deref`-ing to the base so no accessor is duplicated.

use std::ops::{Deref, Range};

use bytes::Bytes;

use crate::draft::{RequestLine, StatusLine};
use crate::header;
use crate::header_table::HeaderTable;
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
/// message, but the error is captured and surfaced on access. A header no
/// element acts on, such as the RFC 6442 location family, is no field here: a
/// reader asks for it through the typed read surface (`msg.header::<H>()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionalHeaders {
    pub p_asserted_identity: Result<Vec<header::NameAddr>, crate::error::SipParseError>,
    pub p_preferred_identity: Result<Vec<header::NameAddr>, crate::error::SipParseError>,
    pub diversion: Result<Vec<header::NameAddr>, crate::error::SipParseError>,
    pub history_info: Result<Vec<header::NameAddr>, crate::error::SipParseError>,
    pub remote_party_id: Result<Vec<header::NameAddr>, crate::error::SipParseError>,
    pub rack: Result<Option<header::RAck>, crate::error::SipParseError>,
    /// Boxed: only a REFER carries one, and a message is moved often.
    pub refer_to: Result<Option<Box<header::ReferTo>>, crate::error::SipParseError>,
}

// ---------------------------------------------------------------------------
// The shared core
// ---------------------------------------------------------------------------

/// The mandatory headers, typed and validated by the parser — so every reader
/// gets them infallibly. The route sets it keeps are derived from the header
/// lines, so they take no part in its equality or its printed form.
#[derive(Clone, PartialEq, Eq)]
pub struct CoreHeaders {
    pub(crate) from: header::From,
    pub(crate) to: header::To,
    pub(crate) call_id: header::CallId,
    pub(crate) cseq: header::CSeq,
    pub(crate) via: NonEmpty<header::Via>,
    pub(crate) contacts: ContactSet,
    pub(crate) routes: ReadRoutes,
}

impl std::fmt::Debug for CoreHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { from, to, call_id, cseq, via, contacts, routes: _ } = self;
        f.debug_struct("CoreHeaders")
            .field("from", from)
            .field("to", to)
            .field("call_id", call_id)
            .field("cseq", cseq)
            .field("via", via)
            .field("contacts", contacts)
            .finish()
    }
}

impl CoreHeaders {
    pub(crate) fn new(
        from: header::From,
        to: header::To,
        call_id: header::CallId,
        cseq: header::CSeq,
        via: NonEmpty<header::Via>,
        contacts: ContactSet,
        routes: ReadRoutes,
    ) -> Self {
        Self { from, to, call_id, cseq, via, contacts, routes }
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

/// The Route and Record-Route entries the parse read and validated, top
/// first, so a reader takes them instead of parsing the lines again; empty
/// where the parse did not read them (a final of 300 or more, a message the
/// stack built). They are derived from the header lines, so they take no part
/// in equality.
#[derive(Debug, Clone, Default)]
pub(crate) enum ReadRoutes {
    /// The parse did not read them.
    #[default]
    Unread,
    /// The parse read them: `None` where the message carries neither header.
    /// Boxed, so a message that is moved often stays narrow.
    Read(Option<Box<RouteSets>>),
}

/// The Route entries, then the Record-Route entries, of one message.
pub(crate) type RouteSets = (Vec<header::RouteEntry>, Vec<header::RecordRouteEntry>);

impl PartialEq for ReadRoutes {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for ReadRoutes {}

/// Everything a request and a response share, written once. Where its body
/// holds the session description is derived from the body and its headers,
/// so it takes no part in its printed form.
#[derive(Clone, PartialEq, Eq)]
pub struct MessageCore {
    /// Full header list in wire order, each line classified — THE source of
    /// truth.
    pub(crate) headers: HeaderTable,
    pub(crate) core: CoreHeaders,
    pub(crate) optional: OptionalHeaders,
    /// Raw body bytes — opaque to the B2BUA. A slice of the image on a parsed
    /// message, so it costs a refcount, not a copy.
    pub(crate) body: Bytes,
    /// The datagram this message is, whether it was received or built.
    pub(crate) image: Bytes,
    /// Where the body holds the session description, located once.
    pub(crate) sdp: Option<Range<u32>>,
}

impl std::fmt::Debug for MessageCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { headers, core, optional, body, image, sdp: _ } = self;
        f.debug_struct("MessageCore")
            .field("headers", headers)
            .field("core", core)
            .field("optional", optional)
            .field("body", body)
            .field("image", image)
            .finish()
    }
}

impl MessageCore {
    pub(crate) fn new(
        headers: HeaderTable,
        core: CoreHeaders,
        optional: OptionalHeaders,
        body: Bytes,
        image: Bytes,
    ) -> Self {
        let sdp = crate::multipart::message_sdp_range(&headers, &body)
            .and_then(|r| Some(u32::try_from(r.start).ok()?..u32::try_from(r.end).ok()?));
        Self { headers, core, optional, body, image, sdp }
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
        self.core().headers.lines()
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
pub struct InDialogRequest<'a> {
    request: &'a SipRequest,
    from_tag: &'a str,
    to_tag: &'a str,
}

impl<'a> InDialogRequest<'a> {
    /// The single validation choke point — the only constructor.
    pub fn new(r: &'a SipRequest) -> Result<Self, NotInDialog> {
        match (r.from().tag(), r.to().tag()) {
            (Some(from_tag), Some(to_tag)) => Ok(Self { request: r, from_tag, to_tag }),
            _ => Err(NotInDialog),
        }
    }
    pub fn from_tag(&self) -> &str {
        self.from_tag
    }
    pub fn to_tag(&self) -> &str {
        self.to_tag
    }
}

impl<'a> Deref for InDialogRequest<'a> {
    type Target = SipRequest;
    fn deref(&self) -> &SipRequest {
        self.request
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
pub struct SipResponseTagged<'a> {
    response: &'a SipResponse,
    to_tag: &'a str,
}

impl<'a> SipResponseTagged<'a> {
    pub fn new(r: &'a SipResponse) -> Result<Self, NotInDialog> {
        let to_tag = r.to().tag().ok_or(NotInDialog)?;
        Ok(Self { response: r, to_tag })
    }
    pub fn to_tag(&self) -> &str {
        self.to_tag
    }
}

impl<'a> Deref for SipResponseTagged<'a> {
    type Target = SipResponse;
    fn deref(&self) -> &SipResponse {
        self.response
    }
}
