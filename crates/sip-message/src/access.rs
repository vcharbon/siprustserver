//! The typed read surface of ADR-0025 on a frozen message.
//!
//! A reader asks the message for the value it wants — `msg.from().tag()`,
//! `msg.top_via().branch()`, `msg.header::<Require>()` — instead of looking a
//! header up by string and re-parsing it. The mandatory values are the ones the
//! parser validated and stored, so they are infallible and borrowed; everything
//! else reads the header line on demand and reports its own parse errors.
//!
//! The surface is written once here and attached to both message types, so
//! neither direction carries a copy of it.

use std::any::Any;

use bytes::Bytes;

use crate::draft::{HeaderList, RequestDraft, ResponseDraft};
use crate::error::SipParseError;
use crate::header::{self, Folding, HeaderName, HeaderValue, Uri};
use crate::header_table::HeaderTable;
use crate::method::Method;
use crate::parser::custom::request_uri::request_uri_refusal;
use crate::sip_str::SipStr;
use crate::types::{
    ContactSet, MessageCore, NonEmpty, OptionalHeaders, SipHeader, SipMessage, SipRequest,
    SipResponse,
};

// ---------------------------------------------------------------------------
// Header-list reads
// ---------------------------------------------------------------------------

fn raw_values(headers: &HeaderTable, name: HeaderName) -> impl Iterator<Item = &str> {
    headers.of(name).map(|h| h.value.as_str())
}

fn raw_text_values(headers: &HeaderTable, name: HeaderName) -> impl Iterator<Item = SipStr> + '_ {
    headers.of(name).map(|h| h.value.clone())
}

fn values_of<H: HeaderValue>(core: &MessageCore) -> Result<Vec<H>, SipParseError> {
    if let Some(read) = read_at_parse::<H>(core) {
        return Ok(read.clone());
    }
    let name = H::header_name();
    let mut values = Vec::new();
    for header in core.headers.of(&name) {
        H::parse_line_into(&header.value, &mut values)?;
    }
    Ok(values)
}

/// The values of `H` the parse already read and validated: the Route and
/// Record-Route sets of a message whose parse reads them.
fn read_at_parse<H: HeaderValue>(core: &MessageCore) -> Option<&Vec<H>> {
    let (routes, recorded) = core.core.routes.sets.as_ref()?;
    let routes: &dyn Any = routes;
    let recorded: &dyn Any = recorded;
    routes.downcast_ref::<Vec<H>>().or_else(|| recorded.downcast_ref::<Vec<H>>())
}

/// The recorded routes, top first. A route whose URI fails the Request-URI
/// checks is no hop any request can be addressed to (RFC 3261 §12.2.1.1), so
/// it refuses the whole set rather than become a dialog's route.
fn recorded_routes(
    core: &MessageCore,
) -> Result<HeaderList<header::RecordRouteEntry>, SipParseError> {
    if let Some(read) = read_at_parse::<header::RecordRouteEntry>(core) {
        return Ok(HeaderList::new(read.clone()));
    }
    let set = values_of::<header::RecordRouteEntry>(core)?;
    for entry in &set {
        request_uri_refusal(entry.uri())?;
    }
    Ok(HeaderList::new(set))
}

/// The single logical value of `H`. A header whose reader sees only the first
/// value ([`HeaderValue::combine`] by default) reads only the first line that
/// carries it; a set-like or comma-folded one reads them all.
fn header_of<H: HeaderValue>(core: &MessageCore) -> Option<Result<H, SipParseError>> {
    if matches!(H::folding(), Folding::Single | Folding::Opaque) {
        let name = H::header_name();
        let first = core.headers.of(&name).next()?;
        return match H::parse_line(&first.value) {
            Err(e) => Some(Err(e)),
            Ok(values) => H::combine(values).map(Ok),
        };
    }
    match values_of::<H>(core) {
        Err(e) => Some(Err(e)),
        Ok(values) if values.is_empty() => None,
        Ok(values) => H::combine(values).map(Ok),
    }
}

/// The read surface every message has, spelled once over the shared core and
/// delegated by both directions.
macro_rules! core_read_surface {
    ($message:ty) => {
        impl $message {
            fn core(&self) -> &MessageCore {
                &self.inner
            }

            /// The originator of the dialog leg.
            pub fn from(&self) -> &header::From {
                &self.core().core.from
            }

            /// The target of the dialog leg.
            pub fn to(&self) -> &header::To {
                &self.core().core.to
            }

            pub fn call_id(&self) -> &header::CallId {
                &self.core().core.call_id
            }

            pub fn cseq(&self) -> &header::CSeq {
                &self.core().core.cseq
            }

            /// Every hop, top first.
            pub fn via(&self) -> &NonEmpty<header::Via> {
                &self.core().core.via
            }

            /// The hop a response to this message goes back through.
            pub fn top_via(&self) -> &header::Via {
                self.core().core.via.first()
            }

            /// Every Contact this message carries.
            pub fn contacts(&self) -> &ContactSet {
                &self.core().core.contacts
            }

            /// The eagerly-parsed optional structured headers.
            pub fn optional(&self) -> &OptionalHeaders {
                &self.core().optional
            }

            /// The full header list in wire order.
            pub fn headers(&self) -> &[SipHeader] {
                self.core().headers.lines()
            }

            /// The header lines with the header each was classified as.
            pub fn header_table(&self) -> &HeaderTable {
                &self.core().headers
            }

            pub fn body(&self) -> &Bytes {
                &self.core().body
            }

            /// The session description this message carries, bare under
            /// `application/sdp` or framed in a `multipart/…` body (RFC 5621
            /// §3.1); `None` where it carries none.
            pub fn sdp(&self) -> Option<&[u8]> {
                let ct = self.header::<header::MediaType>()?.ok()?;
                crate::multipart::sdp_range(&ct, self.body()).and_then(|r| self.body().get(r))
            }

            /// The datagram this message is — received or rendered at freeze.
            pub fn image(&self) -> &Bytes {
                &self.core().image
            }

            /// The single logical value of `H`, or `None` when the message
            /// carries no such header.
            pub fn header<H: HeaderValue>(&self) -> Option<Result<H, SipParseError>> {
                header_of::<H>(self.core())
            }

            /// Every value of `H`, flattening comma folds where the header's
            /// grammar admits them.
            pub fn list<H: HeaderValue>(&self) -> Result<Vec<H>, SipParseError> {
                values_of::<H>(self.core())
            }

            /// The unparsed values of one header, in wire order — the escape
            /// hatch for a header that genuinely stays opaque.
            pub fn raw(&self, name: HeaderName) -> impl Iterator<Item = &str> {
                raw_values(&self.core().headers, name)
            }

            /// The unparsed values of one header as shared text — the seam a
            /// verbatim echo seeds from, copying no bytes.
            pub fn raw_text(&self, name: HeaderName) -> impl Iterator<Item = SipStr> + '_ {
                raw_text_values(&self.core().headers, name)
            }

            pub fn has(&self, name: &HeaderName) -> bool {
                self.core().headers.has(name)
            }

            /// The Route set carried on this message, top first.
            pub fn route_set(&self) -> Result<HeaderList<header::RouteEntry>, SipParseError> {
                Ok(HeaderList::new(self.list::<header::RouteEntry>()?))
            }

            /// The Record-Route set as recorded, top first, refused where a
            /// route is no request's target. The route set a dialog applies
            /// is this list [`reversed`](HeaderList::reversed) (RFC 3261
            /// §12.1.1).
            pub fn record_route_set(
                &self,
            ) -> Result<HeaderList<header::RecordRouteEntry>, SipParseError> {
                recorded_routes(self.core())
            }
        }
    };
}

core_read_surface!(SipRequest);
core_read_surface!(SipResponse);

impl SipRequest {
    pub fn method(&self) -> &Method {
        &self.start.method
    }

    /// The Request-URI — where this request is aimed.
    pub fn request_uri(&self) -> &Uri {
        &self.start.uri
    }

    pub fn version(&self) -> &str {
        self.start.version.as_str()
    }

    /// The editable twin of this request — the only path to a modified message.
    pub fn thaw(&self) -> RequestDraft {
        RequestDraft::thaw(self)
    }

    /// Whether this request offers reliable provisional responses (RFC 3262
    /// §3): `100rel` listed in `Require` (the UAS MUST then answer reliably) or
    /// in `Supported` (it MAY). A line that does not read offers nothing.
    pub fn offers_100rel(&self) -> bool {
        let required = self
            .list::<header::Require>()
            .is_ok_and(|values| values.iter().any(|v| v.contains("100rel")));
        let supported = self
            .list::<header::Supported>()
            .is_ok_and(|values| values.iter().any(|v| v.contains("100rel")));
        required || supported
    }
}

impl SipResponse {
    pub fn status(&self) -> u16 {
        self.start.status
    }

    pub fn reason(&self) -> &str {
        self.start.reason.as_str()
    }

    pub fn version(&self) -> &str {
        self.start.version.as_str()
    }

    pub fn thaw(&self) -> ResponseDraft {
        ResponseDraft::thaw(self)
    }
}

/// The dispatch point forwards to the one core rather than re-implementing it.
impl SipMessage {
    pub fn from(&self) -> &header::From {
        match self {
            SipMessage::Request(r) => r.from(),
            SipMessage::Response(r) => r.from(),
        }
    }

    pub fn to(&self) -> &header::To {
        match self {
            SipMessage::Request(r) => r.to(),
            SipMessage::Response(r) => r.to(),
        }
    }

    pub fn call_id(&self) -> &header::CallId {
        match self {
            SipMessage::Request(r) => r.call_id(),
            SipMessage::Response(r) => r.call_id(),
        }
    }

    pub fn cseq(&self) -> &header::CSeq {
        match self {
            SipMessage::Request(r) => r.cseq(),
            SipMessage::Response(r) => r.cseq(),
        }
    }

    pub fn via(&self) -> &NonEmpty<header::Via> {
        match self {
            SipMessage::Request(r) => r.via(),
            SipMessage::Response(r) => r.via(),
        }
    }

    pub fn top_via(&self) -> &header::Via {
        match self {
            SipMessage::Request(r) => r.top_via(),
            SipMessage::Response(r) => r.top_via(),
        }
    }

    pub fn contacts(&self) -> &ContactSet {
        match self {
            SipMessage::Request(r) => r.contacts(),
            SipMessage::Response(r) => r.contacts(),
        }
    }

    pub fn header<H: HeaderValue>(&self) -> Option<Result<H, SipParseError>> {
        header_of::<H>(self.core())
    }

    pub fn list<H: HeaderValue>(&self) -> Result<Vec<H>, SipParseError> {
        values_of::<H>(self.core())
    }

    pub fn raw(&self, name: HeaderName) -> impl Iterator<Item = &str> {
        raw_values(&self.core().headers, name)
    }

    pub fn raw_text(&self, name: HeaderName) -> impl Iterator<Item = SipStr> + '_ {
        raw_text_values(&self.core().headers, name)
    }

    pub fn has(&self, name: &HeaderName) -> bool {
        self.core().headers.has(name)
    }

    pub fn route_set(&self) -> Result<HeaderList<header::RouteEntry>, SipParseError> {
        Ok(HeaderList::new(self.list::<header::RouteEntry>()?))
    }

    pub fn record_route_set(&self) -> Result<HeaderList<header::RecordRouteEntry>, SipParseError> {
        recorded_routes(self.core())
    }
}

#[cfg(test)]
mod offers_100rel_tests {
    use crate::parser::custom::CustomParser;
    use crate::{SipMessage, SipParser};

    fn request(extra: &str) -> crate::SipRequest {
        let raw = format!(
            "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-a\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: c1\r\n\
CSeq: 1 INVITE\r\n\
{extra}Content-Length: 0\r\n\r\n"
        );
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Request(r) => r,
            _ => panic!("expected request"),
        }
    }

    #[test]
    fn supported_or_require_listing_100rel_offers_it() {
        assert!(request("Supported: timer, 100rel\r\n").offers_100rel());
        assert!(request("Require: 100REL\r\n").offers_100rel(), "option tags are case-insensitive");
        assert!(
            request("Supported: timer\r\nSupported: 100rel\r\n").offers_100rel(),
            "every line counts"
        );
    }

    #[test]
    fn a_request_listing_it_nowhere_offers_nothing() {
        assert!(!request("").offers_100rel());
        assert!(!request("Supported: timer, replaces\r\n").offers_100rel());
        assert!(
            !request("Allow: PRACK\r\n").offers_100rel(),
            "advertising the method is not offering the extension"
        );
    }
}
