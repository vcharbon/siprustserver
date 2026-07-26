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
use crate::sip_str::{SharedText, SipStr};
use crate::types::SipHeader;

use super::entry::Entry;
use super::list::HeaderList;
use super::render::render;
use super::start::StartKind;

/// Why a draft could not become a message.
#[derive(Debug, Clone)]
pub enum IncompleteDraft {
    /// A header RFC 3261 requires on every message of this direction is absent.
    /// Only a blank draft can reach this: a thawed draft started valid and no
    /// operation removes a mandatory header without putting one back.
    Missing(Vec<HeaderName>),
    /// A header is present but does not read as the value its name promises.
    Unreadable(SipParseError),
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
        self.entries.iter().any(|e| e.is(name))
    }

    /// Every line of `name` as text, in wire order.
    pub fn raw(&self, name: &HeaderName) -> Vec<SipStr> {
        self.entries.iter().filter(|e| e.is(name)).map(Entry::text).collect()
    }

    /// Every value of `H` the draft carries.
    pub fn values<H: HeaderValue>(&self) -> Result<Vec<H>, SipParseError> {
        let name = H::header_name();
        let mut values = Vec::new();
        for entry in self.entries.iter().filter(|e| e.is(&name)) {
            values.extend(entry.read::<H>()?);
        }
        Ok(values)
    }

    /// The single logical value of `H`, or `None` when the draft carries none.
    pub fn header<H: HeaderValue>(&self) -> Option<Result<H, SipParseError>> {
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

    /// Put a typed value on the first line.
    pub fn push_front(mut self, value: impl HeaderValue) -> Self {
        self.entries.insert(0, Entry::typed(value));
        self
    }

    /// Append a line whose value is NOT parsed or validated — the escape hatch
    /// for an extension header and for the test lanes that need a deliberately
    /// malformed value.
    pub fn push_raw(mut self, name: HeaderName, value: impl Into<SipStr>) -> Self {
        self.entries.push(Entry::raw(name, value));
        self
    }

    /// Replace every line of this header with one carrying `value`, keeping the
    /// wire position the header already had.
    pub fn set(mut self, value: impl HeaderValue) -> Self {
        let name = value.name();
        match self.entries.iter().position(|e| e.is(&name)) {
            Some(at) => {
                self.entries[at] = Entry::typed(value);
                let mut i = at + 1;
                while i < self.entries.len() {
                    if self.entries[i].is(&name) {
                        self.entries.remove(i);
                    } else {
                        i += 1;
                    }
                }
                self
            }
            None => self.push(value),
        }
    }

    pub fn remove(mut self, name: &HeaderName) -> Self {
        self.entries.retain(|e| !e.is(name));
        self
    }

    /// Keep only the lines whose name passes `keep`.
    pub fn retain(mut self, mut keep: impl FnMut(&HeaderName) -> bool) -> Self {
        self.entries.retain(|e| keep(e.name()));
        self
    }

    /// Edit every value of one header as a single list, in wire order. The
    /// lines are replaced by the resulting values at the position the header
    /// already occupied. A header line that does not read as `H` leaves the
    /// draft untouched.
    pub fn list<H: HeaderValue>(
        mut self,
        f: impl FnOnce(HeaderList<H>) -> HeaderList<H>,
    ) -> Self {
        let name = H::header_name();
        let positions: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.is(&name))
            .map(|(i, _)| i)
            .collect();

        let mut values = Vec::new();
        for &at in &positions {
            match self.entries[at].read::<H>() {
                Ok(read) => values.extend(read),
                Err(_) => return self,
            }
        }

        let updated = f(HeaderList::new(values)).into_vec();
        let at = positions.first().copied().unwrap_or(self.entries.len());
        for &position in positions.iter().rev() {
            self.entries.remove(position);
        }
        let at = at.min(self.entries.len());
        for (offset, value) in updated.into_iter().enumerate() {
            self.entries.insert(at + offset, Entry::typed(value));
        }
        self
    }

    /// Rewrite the first value of one header, parsing that line on this first
    /// touch. A header the draft does not carry, or whose value does not read
    /// as `H`, is left as it stands.
    pub fn update<H: HeaderValue>(self, f: impl FnOnce(H) -> H) -> Self {
        self.list::<H>(|list| list.map_first(f))
    }

    /// Edit the Via list — the hop rewrite every router performs.
    pub fn vias(self, f: impl FnOnce(HeaderList<Via>) -> HeaderList<Via>) -> Self {
        self.list::<Via>(f)
    }

    /// Carry `body`, described by `content_type`.
    pub fn body(mut self, body: Bytes, content_type: MediaType) -> Self {
        self.body = body;
        self.set(content_type)
    }

    pub fn without_body(mut self) -> Self {
        self.body = Bytes::new();
        self.remove(&HeaderName::ContentType)
    }

    // --- leaving the draft ---

    /// The message. Fails only when a mandatory header is absent, or when a
    /// header present on the draft cannot be read as the value its name
    /// promises.
    pub fn freeze(self) -> Result<S::Message, IncompleteDraft> {
        let draft = self.with_content_length();
        let missing: Vec<HeaderName> =
            S::REQUIRED.iter().filter(|name| !draft.has(name)).cloned().collect();
        if !missing.is_empty() {
            return Err(IncompleteDraft::Missing(missing));
        }

        let rendered = render::<S>(&draft.start, &draft.entries, &draft.body);
        let image = decode(&rendered.bytes[..rendered.body_at]);
        let headers: Vec<SipHeader> = rendered
            .headers
            .iter()
            .map(|(name, value)| SipHeader {
                name: image.span(name.0, name.1),
                value: image.span(value.0, value.1),
            })
            .collect();
        let raw = Bytes::from(rendered.bytes);
        let body = raw.slice(rendered.body_at..);
        S::assemble(draft.start, &image, rendered.start, headers, body, raw)
            .map_err(IncompleteDraft::Unreadable)
    }

    /// The wire bytes of this draft in whatever state it is in: no mandatory
    /// header check, no Content-Length correction, no typed message produced.
    /// Invalidity can leave the stack this way and no other, which is what
    /// keeps "a typed message is always valid" true.
    pub fn render_unchecked(self) -> Bytes {
        Bytes::from(render::<S>(&self.start, &self.entries, &self.body).bytes)
    }

    /// Declare the body length the message actually carries. A bodiless message
    /// gets no Content-Length it did not already have — the serializer's
    /// long-standing contract.
    fn with_content_length(self) -> Self {
        let length = self.body.len() as u32;
        if length > 0 || self.has(&HeaderName::ContentLength) {
            self.set(ContentLength::new(length))
        } else {
            self
        }
    }
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
