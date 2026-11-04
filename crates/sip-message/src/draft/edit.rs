//! [`Draft`] — the editable twin of a parsed message, and the only way to make
//! one.
//!
//! Updates consume `self` and return `Self`, so a multi-phase construction
//! splits across plain functions over the draft and still compiles to in-place
//! mutation. `freeze` renders once and hands back a message that carries its
//! own image; `render_unchecked` emits bytes from any state at all, which is
//! how a deliberately invalid datagram leaves the stack without ever existing
//! as a typed message.

use bytes::Bytes;

use crate::error::SipParseError;
use crate::header::{ContentLength, HeaderName, HeaderValue, MediaType, Via};
use crate::header_table::HeaderTable;
use crate::sip_str::{SharedText, SipStr};
use crate::types::SipHeader;
use crate::write_check::WriteRefusal;

use super::entry::Entry;
use super::list::HeaderList;
use super::render::{render, render_bytes};
use super::start::StartKind;

/// Why a draft could not become a message.
#[derive(Debug, Clone)]
pub enum IncompleteDraft {
    /// A header RFC 3261 requires on every message of this direction is absent:
    /// the draft started blank, an edit removed it, or the draft thawed from a
    /// message that arrived without it (a request without Max-Forwards, which
    /// RFC 3261 §16.3 lets a receiver accept).
    Missing(Vec<HeaderName>),
    /// A header is present but does not read as the value its name promises.
    Unreadable(SipParseError),
    /// A line cannot be written whole: a header name off `token`, or a CR or
    /// LF inside the start line or a header value (ADR-0053).
    Unwritable(WriteRefusal),
}

impl std::fmt::Display for IncompleteDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IncompleteDraft::Missing(names) => {
                write!(f, "draft is missing mandatory header(s):")?;
                for name in names {
                    write!(f, " {name}")?;
                }
                Ok(())
            }
            IncompleteDraft::Unreadable(e) => write!(f, "draft header is unreadable: {}", e.reason),
            IncompleteDraft::Unwritable(e) => write!(f, "draft line is unwritable: {e}"),
        }
    }
}

impl std::error::Error for IncompleteDraft {}

/// A message under construction. `S` contributes the start line, the mandatory
/// header set and the frozen message type — nothing else in this file knows
/// whether it is editing a request or a response.
#[derive(Debug)]
pub struct Draft<S: StartKind> {
    start: S::Line,
    entries: Vec<Entry>,
    body: Bytes,
}

impl<S: StartKind> Clone for Draft<S> {
    fn clone(&self) -> Self {
        Self { start: self.start.clone(), entries: self.entries.clone(), body: self.body.clone() }
    }
}

impl<S: StartKind> Draft<S> {
    pub(super) fn from_parts(start: S::Line, entries: Vec<Entry>, body: Bytes) -> Self {
        Self { start, entries, body }
    }

    pub(super) fn start_line(&self) -> &S::Line {
        &self.start
    }

    pub(super) fn map_start(mut self, f: impl FnOnce(S::Line) -> S::Line) -> Self {
        self.start = f(self.start);
        self
    }

    // --- reading ---

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn has(&self, name: &HeaderName) -> bool {
        let key = name.key();
        self.entries.iter().any(|e| e.is_key(&key))
    }

    /// Every line of `name` as text, in wire order.
    pub fn raw(&self, name: &HeaderName) -> Vec<SipStr> {
        let key = name.key();
        self.entries.iter().filter(|e| e.is_key(&key)).map(Entry::text).collect()
    }

    /// Every value of `H` the draft carries.
    pub fn values<H: HeaderValue>(&self) -> Result<Vec<H>, SipParseError> {
        let name = H::header_name();
        let key = name.key();
        let mut values = Vec::new();
        for entry in self.entries.iter().filter(|e| e.is_key(&key)) {
            values.extend(entry.read::<H>()?);
        }
        Ok(values)
    }

    /// The single logical value of `H`, or `None` when the draft carries none.
    /// A header read from its first line only
    /// ([`HeaderValue::first_line_only`]) is that line's value, whatever a
    /// later line of it holds, malformed or not, as on a message.
    pub fn header<H: HeaderValue>(&self) -> Option<Result<H, SipParseError>> {
        if H::first_line_only() {
            let name = H::header_name();
            let key = name.key();
            let first = self.entries.iter().find(|e| e.is_key(&key))?;
            return match first.read::<H>() {
                Err(e) => Some(Err(e)),
                Ok(values) => H::combine(values).map(Ok),
            };
        }
        match self.values::<H>() {
            Err(e) => Some(Err(e)),
            Ok(values) if values.is_empty() => None,
            Ok(values) => H::combine(values).map(Ok),
        }
    }

    pub fn body_bytes(&self) -> &Bytes {
        &self.body
    }

    // --- editing ---

    /// Append a typed value as its own line.
    pub fn push(mut self, value: impl HeaderValue) -> Self {
        self.entries.push(Entry::typed(value));
        self
    }

    /// Append a line the caller already holds as an [`Entry`] — typed or
    /// verbatim. The seam for a caller that decides per line whether it states
    /// a value or echoes bytes.
    pub fn push_entry(mut self, entry: Entry) -> Self {
        self.entries.push(entry);
        self
    }

    /// Put a typed value on the first line of the whole message.
    pub fn push_front(mut self, value: impl HeaderValue) -> Self {
        insert_entry(&mut self.entries, 0, Entry::typed(value));
        self
    }

    /// Put a typed value on its own line immediately above the lines this
    /// header already has — where a router's own Via and Route go. Nothing is
    /// read, so a lower line this hop has no business parsing cannot fail the
    /// edit; [`list`](Self::list) is the way in when the existing values matter.
    pub fn prepend(mut self, value: impl HeaderValue) -> Self {
        let name = value.name();
        let key = name.key();
        let at = self.entries.iter().position(|e| e.is_key(&key)).unwrap_or(self.entries.len());
        insert_entry(&mut self.entries, at, Entry::typed(value));
        self
    }

    /// Append a line whose value is carried verbatim: not parsed, memcpy'd at
    /// freeze exactly as it was given. This is how a
    /// value the model has no type for reaches the wire — an extension header,
    /// a line echoed octet for octet from the message being answered or
    /// relayed, an option a caller states as text — and it is the seam every
    /// generator recipe lowers such a value onto.
    ///
    /// `freeze` still gates the mandatory headers and reads the ones the typed
    /// core comes from, so a raw line in one of those, or one holding a CR or
    /// LF, fails the freeze; a value
    /// that must go out invalid leaves through
    /// [`render_unchecked`](Self::render_unchecked). A line from outside the
    /// stack goes in through [`Entry::checked_raw`], which refuses it before
    /// it is written, so only that line is lost.
    pub fn push_raw(mut self, name: HeaderName, value: impl Into<SipStr>) -> Self {
        self.entries.push(Entry::raw(name, value));
        self
    }

    /// Replace every line of this header with one carrying `value`, keeping the
    /// wire position the header already had.
    pub fn set(mut self, value: impl HeaderValue) -> Self {
        let name = value.name();
        let key = name.key();
        let Some(at) = self.entries.iter().position(|e| e.is_key(&key)) else {
            return self.push(value);
        };
        self.entries.retain(|e| !e.is_key(&key));
        insert_entry(&mut self.entries, at, Entry::typed(value));
        self
    }

    pub fn remove(mut self, name: &HeaderName) -> Self {
        let key = name.key();
        self.entries.retain(|e| !e.is_key(&key));
        self
    }

    /// Keep only the lines whose name passes `keep`.
    pub fn retain(mut self, mut keep: impl FnMut(&HeaderName) -> bool) -> Self {
        self.entries.retain(|e| keep(e.name()));
        self
    }

    /// Edit every value of one header as a single list, in wire order. The
    /// lines are replaced by the resulting values at the position the header
    /// already occupied; a header the draft does not carry starts as an empty
    /// list.
    ///
    /// Fallible because the edits it exists for are the routing-critical ones —
    /// the hop's own Via, the Max-Forwards decrement, popping the top Route. A
    /// line that does not read as `H` must stop the message, not silently leave
    /// it unedited: a request forwarded with no Via of its own has no path back
    /// for the response, and one forwarded undecremented has lost its loop
    /// bound.
    pub fn list<H: HeaderValue>(
        mut self,
        f: impl FnOnce(HeaderList<H>) -> HeaderList<H>,
    ) -> Result<Self, SipParseError> {
        let name = H::header_name();
        let key = name.key();
        let mut values = Vec::new();
        for entry in self.entries.iter().filter(|e| e.is_key(&key)) {
            values.extend(entry.read::<H>()?);
        }

        let updated = f(HeaderList::new(values)).into_vec();
        // The lines above the first one of this header are none of it, so the
        // first one's position survives taking them all out.
        let at = self.entries.iter().position(|e| e.is_key(&key)).unwrap_or(self.entries.len());
        self.entries.retain(|e| !e.is_key(&key));
        replace_entries(&mut self.entries, at..at, updated.into_iter().map(Entry::typed));
        Ok(self)
    }

    /// Rewrite the first value of one header, reading every line the header
    /// has. A header the draft does not carry is left as it stands; a line that
    /// does not read as `H` is an error, per [`list`](Self::list).
    pub fn update<H: HeaderValue>(self, f: impl FnOnce(H) -> H) -> Result<Self, SipParseError> {
        self.list::<H>(|list| list.map_first(f))
    }

    /// Rewrite the first value of one header, reading only the LINE that
    /// carries it — a router stamps its own hop without parsing hops it has no
    /// business parsing. Lines below the first are left byte-untouched.
    pub fn update_top<H: HeaderValue>(self, f: impl FnOnce(H) -> H) -> Result<Self, SipParseError> {
        self.edit_top_line::<H>(|list| list.map_first(f))
    }

    /// Drop the first value of one header — the RFC 3261 §7.3.1 aware entry
    /// pop. A comma-folded line keeps the values below the one removed; a line
    /// carrying only that value goes away. Only that line is read.
    pub fn pop_top<H: HeaderValue>(self) -> Result<Self, SipParseError> {
        self.edit_top_line::<H>(|mut list| {
            list.pop_front();
            list
        })
    }

    /// Replace the first line of one header with the values `f` yields, at the
    /// position that line held. Every other line is left as it stands.
    fn edit_top_line<H: HeaderValue>(
        mut self,
        f: impl FnOnce(HeaderList<H>) -> HeaderList<H>,
    ) -> Result<Self, SipParseError> {
        let name = H::header_name();
        let key = name.key();
        let Some((at, entry)) = self.entries.iter().enumerate().find(|(_, e)| e.is_key(&key))
        else {
            return Ok(self);
        };
        let values = entry.read::<H>()?;
        let updated = f(HeaderList::new(values));
        replace_entries(
            &mut self.entries,
            at..at.saturating_add(1),
            updated.into_iter().map(Entry::typed),
        );
        Ok(self)
    }

    /// Edit the Via list — the hop rewrite every router performs. Adding this
    /// hop's own Via needs no reading and so cannot fail: that is
    /// [`prepend`](Self::prepend).
    pub fn vias(
        self,
        f: impl FnOnce(HeaderList<Via>) -> HeaderList<Via>,
    ) -> Result<Self, SipParseError> {
        self.list::<Via>(f)
    }

    /// Carry `body`, described by `content_type`.
    pub fn body(self, body: Bytes, content_type: MediaType) -> Self {
        self.with_body(body).set(content_type)
    }

    /// Carry `body` and leave the media type alone — for a recipe that has
    /// already stated the type, or is about to.
    pub fn with_body(mut self, body: Bytes) -> Self {
        self.body = body;
        self
    }

    pub fn without_body(mut self) -> Self {
        self.body = Bytes::new();
        self.remove(&HeaderName::ContentType)
    }

    // --- leaving the draft ---

    /// The message. Fails only when a mandatory header is absent, when a
    /// header present on the draft cannot be read as the value its name
    /// promises, or when a line cannot be written whole.
    pub fn freeze(self) -> Result<S::Message, IncompleteDraft> {
        self.complete()?.rendered()
    }

    /// The message a draft thawed from a RECEIVED message is once edited: it
    /// keeps the header lines the peer sent, so the mandatory set of a message
    /// this stack mints is not asked of it, and its body is the one received.
    /// Fails, as [`freeze`](Self::freeze), on a header that cannot be read
    /// as its name promises or a line that cannot be written whole.
    pub(crate) fn refreeze_received(self) -> Result<S::Message, IncompleteDraft> {
        self.rendered()
    }

    /// The single render of a draft and the typed message read back from it.
    fn rendered(self) -> Result<S::Message, IncompleteDraft> {
        let draft = self;
        let rendered = render::<S>(&draft.start, &draft.entries, &draft.body);
        if let Some(refusal) = rendered.refusal {
            return Err(IncompleteDraft::Unwritable(refusal));
        }
        let outside = || {
            IncompleteDraft::Unreadable(SipParseError::new(
                "the render's body offset is past its bytes",
            ))
        };
        let image = decode(rendered.bytes.get(..rendered.body_at).ok_or_else(outside)?);
        let lines: Vec<SipHeader> = rendered
            .headers
            .iter()
            .map(|(name, value)| SipHeader {
                name: image.span(name.0, name.1),
                value: image.span(value.0, value.1),
            })
            .collect();
        let ids = draft.entries.iter().map(|e| e.id()).collect();
        let headers = HeaderTable::classified(lines, ids);
        let raw = Bytes::from(rendered.bytes);
        let body = crate::checked::slice(&raw, rendered.body_at..raw.len()).ok_or_else(outside)?;
        S::assemble(draft.start, &image, rendered.start, headers, body, raw)
            .map_err(IncompleteDraft::Unreadable)
    }

    /// The wire bytes of a message this draft is complete enough to BE: the
    /// same mandatory-header check and the same single render as
    /// [`freeze`](Self::freeze), without assembling the typed message a relay
    /// never reads. Byte-identical to `freeze()?.raw`.
    pub fn freeze_bytes(self) -> Result<Bytes, IncompleteDraft> {
        let draft = self.complete()?;
        render_bytes::<S>(&draft.start, &draft.entries, &draft.body)
            .map(Bytes::from)
            .map_err(IncompleteDraft::Unwritable)
    }

    /// The wire bytes of this draft in whatever state it is in: no mandatory
    /// header check, no line check, no Content-Length correction, no typed
    /// message produced.
    /// Invalidity can leave the stack this way and no other, which is what
    /// keeps "a typed message is always valid" true.
    ///
    /// SUT paths freeze; this is the test lanes' seam — the peer-side
    /// `allow_violation` scenarios, the RFC-compliance invalid corpus, the
    /// deviation fixtures — and the reviewable grep target that says so.
    pub fn render_unchecked(self) -> Bytes {
        Bytes::from(render::<S>(&self.start, &self.entries, &self.body).bytes)
    }

    /// This draft with its Content-Length restated, once every header RFC 3261
    /// requires of this direction is present.
    fn complete(self) -> Result<Self, IncompleteDraft> {
        let draft = self.with_content_length()?;
        let missing: Vec<HeaderName> =
            S::REQUIRED.iter().filter(|name| !draft.has(name)).cloned().collect();
        if missing.is_empty() {
            Ok(draft)
        } else {
            Err(IncompleteDraft::Missing(missing))
        }
    }

    /// Declare the body length the message actually carries. A bodiless message
    /// gets no Content-Length it did not already have — the serializer's
    /// long-standing contract. A body longer than a `u32` states no length.
    fn with_content_length(self) -> Result<Self, IncompleteDraft> {
        let length = u32::try_from(self.body.len()).map_err(|_| {
            IncompleteDraft::Unreadable(SipParseError::new(
                "body longer than a Content-Length states",
            ))
        })?;
        Ok(if length > 0 || self.has(&HeaderName::ContentLength) {
            self.set(ContentLength::new(length))
        } else {
            self
        })
    }
}

/// Puts `entry` at `at`, the lines from `at` on moving down one; past the end
/// it goes last.
fn insert_entry(entries: &mut Vec<Entry>, at: usize, entry: Entry) {
    let at = at.min(entries.len());
    replace_entries(entries, at..at, std::iter::once(entry));
}

/// Replaces the lines in `range` by `with`, moving the lines after them once
/// whatever the number of lines put in. A range past the end appends.
fn replace_entries(
    entries: &mut Vec<Entry>,
    range: std::ops::Range<usize>,
    with: impl IntoIterator<Item = Entry>,
) {
    let end = range.end.min(entries.len());
    let start = range.start.min(end);
    entries.splice(start..end, with);
}

/// The rendered header block as one shared text image. Everything written came
/// from a `&str`, so the validation always succeeds; the lossy path exists so a
/// caller-supplied raw entry can never panic a freeze.
fn decode(header_block: &[u8]) -> SharedText {
    match std::str::from_utf8(header_block) {
        Ok(s) => SharedText::new(s),
        Err(_) => SharedText::from(String::from_utf8_lossy(header_block).into_owned()),
    }
}
