//! How a draft starts: blank (origination) or seeded from a parsed message
//! (relay), optionally filtered.
//!
//! Seeding copies no text — every entry is a span of the source image, so
//! thawing a message costs refcount bumps. Entries pointing into *different*
//! images coexist in one draft; freeze memcpies each from wherever it lives.

use bytes::Bytes;

use crate::header::{HeaderName, Uri};
use crate::method::Method;
use crate::sip_str::SipStr;
use crate::types::{SipHeader, SipRequest, SipResponse};

use super::edit::Draft;
use super::entry::Entry;
use super::start::{kind, RequestLine, StatusLine, SIP_VERSION};

/// A request under construction.
pub type RequestDraft = Draft<kind::Request>;
/// A response under construction.
pub type ResponseDraft = Draft<kind::Response>;

/// The header count a blank draft is sized for: enough that an ordinary
/// origination fills its entry list without a regrowth.
const TYPICAL_HEADERS: usize = 12;

fn seed(headers: &[SipHeader], keep: impl Fn(&HeaderName) -> bool) -> Vec<Entry> {
    // Sized for the whole header list up front: a relay thaws every message it
    // forwards, so the entry list must not grow its way there.
    let mut entries = Vec::with_capacity(headers.len());
    for header in headers {
        let name = HeaderName::of(&header.name);
        if keep(&name) {
            entries.push(Entry::raw(name, header.value.clone()));
        }
    }
    entries
}

impl RequestDraft {
    /// A blank request — origination. Mandatory headers are the caller's to
    /// supply; [`freeze`](Draft::freeze) says which are missing.
    pub fn new(method: Method, uri: Uri) -> Self {
        Self::from_parts(
            RequestLine { method, uri, version: SipStr::from_static(SIP_VERSION) },
            Vec::with_capacity(TYPICAL_HEADERS),
            Bytes::new(),
        )
    }

    /// The editable twin of a parsed request, header for header.
    pub fn thaw(msg: &SipRequest) -> Self {
        Self::keep(msg, |_| true)
    }

    /// [`thaw`](Self::thaw) keeping only the headers `keep` accepts — the
    /// passthrough filter, expressed as a predicate over header identity
    /// instead of an array of name strings.
    pub fn keep(msg: &SipRequest, keep: impl Fn(&HeaderName) -> bool) -> Self {
        Self::from_parts(
            RequestLine {
                method: msg.method().clone(),
                uri: msg.request_uri().clone(),
                version: SipStr::owned(msg.version()),
            },
            seed(msg.headers(), keep),
            msg.body().clone(),
        )
    }

    pub fn method(&self) -> &Method {
        &self.start_line().method
    }

    /// The Request-URI — where this request is aimed.
    pub fn uri(&self) -> &Uri {
        &self.start_line().uri
    }

    pub fn with_method(self, method: Method) -> Self {
        self.map_start(|line| RequestLine { method, ..line })
    }

    pub fn with_uri(self, uri: Uri) -> Self {
        self.map_start(|line| RequestLine { uri, ..line })
    }
}

impl ResponseDraft {
    /// A blank response.
    pub fn new(status: u16, reason: impl Into<SipStr>) -> Self {
        Self::from_parts(
            StatusLine { version: SipStr::from_static(SIP_VERSION), status, reason: reason.into() },
            Vec::with_capacity(TYPICAL_HEADERS),
            Bytes::new(),
        )
    }

    /// The editable twin of a parsed response.
    pub fn thaw(msg: &SipResponse) -> Self {
        Self::keep(msg, |_| true)
    }

    pub fn keep(msg: &SipResponse, keep: impl Fn(&HeaderName) -> bool) -> Self {
        Self::from_parts(
            StatusLine {
                version: SipStr::owned(msg.version()),
                status: msg.status(),
                reason: SipStr::owned(msg.reason()),
            },
            seed(msg.headers(), keep),
            msg.body().clone(),
        )
    }

    pub fn status(&self) -> u16 {
        self.start_line().status
    }

    pub fn reason(&self) -> &str {
        self.start_line().reason.as_str()
    }

    pub fn with_status(self, status: u16, reason: impl Into<SipStr>) -> Self {
        let reason = reason.into();
        self.map_start(|line| StatusLine { status, reason, ..line })
    }
}
