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

use bytes::Bytes;

use crate::draft::{HeaderList, RequestDraft, ResponseDraft};
use crate::error::SipParseError;
use crate::header::{self, HeaderName, HeaderValue, Uri};
use crate::method::Method;
use crate::sip_str::SipStr;
use crate::types::{
    ContactSet, MessageCore, NonEmpty, OptionalHeaders, SipHeader, SipMessage, SipRequest,
    SipResponse,
};

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
                &self.core().headers
            }

            pub fn body(&self) -> &Bytes {
                &self.core().body
            }

            /// The datagram this message is — received or rendered at freeze.
            pub fn image(&self) -> &Bytes {
                &self.core().image
            }

            /// The single logical value of `H`, or `None` when the message
            /// carries no such header.
            pub fn header<H: HeaderValue>(&self) -> Option<Result<H, SipParseError>> {
                header_of::<H>(self.headers())
            }

            /// Every value of `H`, flattening comma folds where the header's
            /// grammar admits them.
            pub fn list<H: HeaderValue>(&self) -> Result<Vec<H>, SipParseError> {
                values_of::<H>(self.headers())
            }

            /// The unparsed values of one header, in wire order — the escape
            /// hatch for a header that genuinely stays opaque.
            pub fn raw(&self, name: HeaderName) -> impl Iterator<Item = &str> {
                raw_values(self.headers(), name)
            }

            /// The unparsed values of one header as shared text — the seam a
            /// verbatim echo seeds from, copying no bytes.
            pub fn raw_text(&self, name: HeaderName) -> impl Iterator<Item = SipStr> + '_ {
                raw_text_values(self.headers(), name)
            }

            pub fn has(&self, name: &HeaderName) -> bool {
                self.headers().iter().any(|h| name.matches(&h.name))
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

            /// Strict header-content validation — the opt-in re-parse pass.
            pub fn validate_strict(&self) -> Result<(), SipParseError> {
                crate::parser::custom::optional_headers::run_all_strict(self.headers())
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

    pub fn route_set(&self) -> Result<HeaderList<header::RouteEntry>, SipParseError> {
        Ok(HeaderList::new(self.list::<header::RouteEntry>()?))
    }

    pub fn record_route_set(&self) -> Result<HeaderList<header::RecordRouteEntry>, SipParseError> {
        Ok(HeaderList::new(self.list::<header::RecordRouteEntry>()?))
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
