//! The typed read surface of ADR-0025 on a parsed message.
//!
//! A reader asks the message for the value it wants — `msg.from().tag()`,
//! `msg.top_via().branch()`, `msg.header::<Require>()` — instead of looking a
//! header up by string and re-parsing it. The mandatory values are derived from
//! the fields the parser already validated, so they are infallible; everything
//! else reads the header line on demand and reports its own parse errors.

use crate::draft::{HeaderList, RequestDraft, ResponseDraft};
use crate::error::SipParseError;
use crate::header::{self, HeaderName, HeaderValue, HostPort, NameAddr, ParamValue, Uri};
use crate::sip_str::SipStr;
use crate::types::{self, NonEmpty, SipHeader, SipMessage, SipRequest, SipResponse};

// ---------------------------------------------------------------------------
// Header-list reads
// ---------------------------------------------------------------------------

fn raw_values<'a>(
    headers: &'a [SipHeader],
    name: HeaderName,
) -> impl Iterator<Item = &'a str> + 'a {
    headers.iter().filter(move |h| name.matches(&h.name)).map(|h| h.value.as_str())
}

fn raw_text_values<'a>(
    headers: &'a [SipHeader],
    name: HeaderName,
) -> impl Iterator<Item = SipStr> + 'a {
    headers.iter().filter(move |h| name.matches(&h.name)).map(|h| h.value.clone())
}

fn values_of<H: HeaderValue>(headers: &[SipHeader]) -> Result<Vec<H>, SipParseError> {
    let name = H::header_name();
    let mut values = Vec::new();
    for header in headers.iter().filter(|h| name.matches(&h.name)) {
        values.extend(H::parse_line(&header.value)?);
    }
    Ok(values)
}

fn header_of<H: HeaderValue>(headers: &[SipHeader]) -> Option<Result<H, SipParseError>> {
    match values_of::<H>(headers) {
        Err(e) => Some(Err(e)),
        Ok(values) if values.is_empty() => None,
        Ok(values) => H::combine(values).map(Ok),
    }
}

// ---------------------------------------------------------------------------
// Old typed field -> new value. Total: the parser already gated these, so a URI
// it accepted but the strict reader rejects is kept whole rather than lost.
// ---------------------------------------------------------------------------

fn param_value(old: &types::ParamValue) -> ParamValue {
    match old {
        types::ParamValue::Flag => ParamValue::Flag,
        types::ParamValue::Value(v) => ParamValue::text(v.clone()),
    }
}

fn name_addr(old: &types::NameAddr) -> NameAddr {
    let mut addr = NameAddr::new(Uri::parse_or_opaque(&old.uri));
    if let Some(display) = &old.display_name {
        addr = addr.with_display(display.clone());
    }
    for (name, value) in old.params.iter() {
        addr = addr.with_param(name.clone(), param_value(value));
    }
    addr
}

fn via(old: &types::Via) -> header::Via {
    let mut hop = header::Via::new(
        old.transport.clone(),
        HostPort::new(old.host.clone(), old.port),
    );
    for (name, value) in old.params.iter() {
        hop = hop.with_param(name.clone(), param_value(value));
    }
    hop
}

fn vias(old: &NonEmpty<types::Via>) -> NonEmpty<header::Via> {
    let mut it = old.iter().map(via);
    let head = it.next().expect("NonEmpty invariant: at least one Via");
    NonEmpty::from_parts(head, it.collect())
}

/// The read surface shared by requests and responses. Written once here and
/// attached to both message types, so neither direction carries a copy.
macro_rules! typed_read_surface {
    ($message:ty) => {
        impl $message {
            /// The originator of the dialog leg.
            pub fn from(&self) -> header::From {
                header::From::new(name_addr(&self.from))
            }

            /// The target of the dialog leg.
            pub fn to(&self) -> header::To {
                header::To::new(name_addr(&self.to))
            }

            pub fn call_id(&self) -> header::CallId {
                header::CallId::new(self.call_id.clone())
            }

            pub fn cseq(&self) -> header::CSeq {
                header::CSeq::new(self.cseq.seq, self.cseq.method.clone())
            }

            /// Every hop, top first.
            pub fn via(&self) -> NonEmpty<header::Via> {
                vias(&self.via)
            }

            /// The hop a response to this message goes back through.
            pub fn top_via(&self) -> header::Via {
                via(self.via.first())
            }

            /// The single logical value of `H`, or `None` when the message
            /// carries no such header.
            pub fn header<H: HeaderValue>(&self) -> Option<Result<H, SipParseError>> {
                header_of::<H>(&self.headers)
            }

            /// Every value of `H`, flattening comma folds where the header's
            /// grammar admits them.
            pub fn list<H: HeaderValue>(&self) -> Result<Vec<H>, SipParseError> {
                values_of::<H>(&self.headers)
            }

            /// The unparsed values of one header, in wire order — the escape
            /// hatch for a header that genuinely stays opaque.
            pub fn raw(&self, name: HeaderName) -> impl Iterator<Item = &str> {
                raw_values(&self.headers, name)
            }

            /// The unparsed values of one header as shared text — the seam a
            /// verbatim echo seeds from, copying no bytes.
            pub fn raw_text(&self, name: HeaderName) -> impl Iterator<Item = SipStr> + '_ {
                raw_text_values(&self.headers, name)
            }

            pub fn has(&self, name: &HeaderName) -> bool {
                self.headers.iter().any(|h| name.matches(&h.name))
            }

            /// The Route set carried on this message, top first.
            pub fn route_set(&self) -> Result<HeaderList<header::RouteEntry>, SipParseError> {
                Ok(HeaderList::new(self.list::<header::RouteEntry>()?))
            }

            /// The Record-Route set as recorded, top first. The route set a
            /// dialog applies is this list [`reversed`](HeaderList::reversed)
            /// (RFC 3261 §12.1.1).
            pub fn record_route_set(
                &self,
            ) -> Result<HeaderList<header::RecordRouteEntry>, SipParseError> {
                Ok(HeaderList::new(self.list::<header::RecordRouteEntry>()?))
            }
        }
    };
}

typed_read_surface!(SipRequest);
typed_read_surface!(SipResponse);

impl SipRequest {
    /// The editable twin of this request — the only path to a modified message.
    pub fn thaw(&self) -> RequestDraft {
        RequestDraft::thaw(self)
    }

    /// The Request-URI, structured.
    pub fn request_uri(&self) -> Uri {
        Uri::parse_or_opaque(&self.uri)
    }
}

impl SipResponse {
    pub fn thaw(&self) -> ResponseDraft {
        ResponseDraft::thaw(self)
    }
}

impl SipMessage {
    pub fn from(&self) -> header::From {
        match self {
            SipMessage::Request(r) => r.from(),
            SipMessage::Response(r) => r.from(),
        }
    }

    pub fn to(&self) -> header::To {
        match self {
            SipMessage::Request(r) => r.to(),
            SipMessage::Response(r) => r.to(),
        }
    }

    pub fn call_id(&self) -> header::CallId {
        match self {
            SipMessage::Request(r) => r.call_id(),
            SipMessage::Response(r) => r.call_id(),
        }
    }

    pub fn cseq(&self) -> header::CSeq {
        match self {
            SipMessage::Request(r) => r.cseq(),
            SipMessage::Response(r) => r.cseq(),
        }
    }

    pub fn via(&self) -> NonEmpty<header::Via> {
        match self {
            SipMessage::Request(r) => r.via(),
            SipMessage::Response(r) => r.via(),
        }
    }

    pub fn top_via(&self) -> header::Via {
        match self {
            SipMessage::Request(r) => r.top_via(),
            SipMessage::Response(r) => r.top_via(),
        }
    }

    pub fn header<H: HeaderValue>(&self) -> Option<Result<H, SipParseError>> {
        header_of::<H>(self.headers())
    }

    pub fn list<H: HeaderValue>(&self) -> Result<Vec<H>, SipParseError> {
        values_of::<H>(self.headers())
    }

    pub fn raw(&self, name: HeaderName) -> impl Iterator<Item = &str> {
        raw_values(self.headers(), name)
    }

    pub fn raw_text(&self, name: HeaderName) -> impl Iterator<Item = SipStr> + '_ {
        raw_text_values(self.headers(), name)
    }

    pub fn has(&self, name: &HeaderName) -> bool {
        self.headers().iter().any(|h| name.matches(&h.name))
    }
}
