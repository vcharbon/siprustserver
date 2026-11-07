//! A value wrapped in a whole message, and the production path that reads it:
//! the wire parser at its default limits, at first ingress, then the typed
//! reader of the header.

use sip_message::header::{self, HeaderName, HeaderValue};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParseError, SipParser, SipParserLimits};

/// A reader of one header of an accepted message.
pub type ReadCheck = fn(&SipMessage) -> Result<(), String>;

/// A message of named lines: the line of a name can be replaced, and any other
/// added after them.
#[derive(Clone)]
pub struct Template {
    start: Vec<u8>,
    lines: Vec<(&'static str, Vec<u8>)>,
    body: Vec<u8>,
    frame: bool,
}

/// A method whose request can establish a dialog (RFC 3261 §8.1.1.8, RFC 6665,
/// RFC 3515).
fn opens_dialog(method: &str) -> bool {
    matches!(method, "INVITE" | "SUBSCRIBE" | "NOTIFY" | "REFER")
}

const VIA: &str = "Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-conformance";

impl Template {
    /// A valid new request of `method`, no To-tag.
    pub fn request(method: &str) -> Self {
        let mut lines = vec![
            ("via", VIA.into()),
            ("max-forwards", "Max-Forwards: 70".into()),
            ("from", "From: <sip:alice@example.com>;tag=a1".into()),
            ("to", "To: <sip:bob@example.com>".into()),
            ("call-id", "Call-ID: conformance-1@192.0.2.10".into()),
            ("cseq", format!("CSeq: 1 {method}").into_bytes()),
        ];
        if opens_dialog(method) {
            lines.push(("contact", "Contact: <sip:alice@192.0.2.10:5060>".into()));
        }
        Self {
            start: format!("{method} sip:bob@example.com SIP/2.0").into_bytes(),
            lines,
            body: Vec::new(),
            frame: true,
        }
    }

    /// A valid response of `status` to a request of `method`.
    pub fn response(status: u16, method: &str) -> Self {
        let mut lines = vec![
            ("via", VIA.into()),
            ("from", "From: <sip:alice@example.com>;tag=a1".into()),
            ("to", "To: <sip:bob@example.com>;tag=b1".into()),
            ("call-id", "Call-ID: conformance-1@192.0.2.10".into()),
            ("cseq", format!("CSeq: 1 {method}").into_bytes()),
        ];
        if (200..300).contains(&status) && opens_dialog(method) && method != "NOTIFY" {
            lines.push(("contact", "Contact: <sip:bob@192.0.2.20:5060>".into()));
        }
        Self {
            start: format!("SIP/2.0 {status} Reason").into_bytes(),
            lines,
            body: Vec::new(),
            frame: true,
        }
    }

    /// `line` in place of the line named `name`, or after every other line.
    pub fn with(mut self, name: &'static str, line: impl Into<Vec<u8>>) -> Self {
        let line = line.into();
        match self.lines.iter_mut().find(|(known, _)| *known == name) {
            Some(slot) => slot.1 = line,
            None => self.lines.push((name, line)),
        }
        self
    }

    /// `line` after every other line, whatever its name.
    pub fn add(mut self, line: impl Into<Vec<u8>>) -> Self {
        self.lines.push(("", line.into()));
        self
    }

    /// `start` as the start line.
    pub fn start_line(mut self, start: impl Into<Vec<u8>>) -> Self {
        self.start = start.into();
        self
    }

    /// `body` after the blank line, which the Content-Length line frames.
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// No Content-Length line of the template's own: a line the caller added
    /// frames the message, or the datagram does (RFC 3261 §18.3).
    pub fn unframed(mut self) -> Self {
        self.frame = false;
        self
    }

    /// The datagram.
    pub fn bytes(&self) -> Vec<u8> {
        let mut out = self.head();
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&self.body);
        out
    }

    /// The datagram with no blank line after its last header: every line ends
    /// in CRLF and nothing follows.
    pub fn bytes_without_blank_line(&self) -> Vec<u8> {
        self.head()
    }

    fn head(&self) -> Vec<u8> {
        let mut out = self.start.clone();
        out.extend_from_slice(b"\r\n");
        for (_, line) in &self.lines {
            out.extend_from_slice(line);
            out.extend_from_slice(b"\r\n");
        }
        if self.frame {
            out.extend_from_slice(format!("Content-Length: {}\r\n", self.body.len()).as_bytes());
        }
        out
    }
}

/// The parser a party's new request meets at its first hop, at the default
/// limits and with no exemption.
pub fn production() -> CustomParser {
    CustomParser::with_limits(SipParserLimits { at_first_ingress: true, ..Default::default() })
}

/// `raw` through the production parser.
pub fn read(raw: &[u8]) -> Result<SipMessage, SipParseError> {
    production().parse(raw)
}

fn lines_of(msg: &SipMessage, name: HeaderName) -> usize {
    match msg {
        SipMessage::Request(r) => r.raw_text(name).count(),
        SipMessage::Response(r) => r.raw_text(name).count(),
    }
}

/// `H`'s typed reader over an accepted message: its lines read under their
/// canonical name, and every value reads.
pub fn typed<H: HeaderValue>(msg: &SipMessage) -> Result<(), String> {
    if lines_of(msg, H::header_name()) == 0 {
        return Err(format!("no {} line under its canonical name", H::header_name()));
    }
    msg.list::<H>().map(|_| ()).map_err(|e| e.reason)?;
    match msg.header::<H>() {
        Some(Err(e)) => Err(e.reason),
        Some(Ok(_)) | None => Ok(()),
    }
}

/// A header no typed reader serves: its lines read under their canonical name.
pub fn raw(msg: &SipMessage, name: &str) -> Result<(), String> {
    match lines_of(msg, HeaderName::from(name)) {
        0 => Err(format!("no {name} line under its canonical name")),
        _ => Ok(()),
    }
}

macro_rules! every_typed_read {
    ($msg:expr, $($ty:ty),* $(,)?) => {
        $( if lines_of($msg, <$ty>::header_name()) > 0 { typed::<$ty>($msg)?; } )*
    };
}

/// Every typed reader of a header `msg` holds: a valid message reads whole.
pub fn reads_every_header(msg: &SipMessage) -> Result<(), String> {
    every_typed_read!(
        msg,
        header::From,
        header::To,
        header::Contact,
        header::RouteEntry,
        header::RecordRouteEntry,
        header::PathEntry,
        header::ServiceRouteEntry,
        header::ReferTo,
        header::ReferredBy,
        header::ReplyTo,
        header::Diversion,
        header::HistoryInfo,
        header::RemotePartyId,
        header::PAssertedIdentity,
        header::PPreferredIdentity,
        header::Require,
        header::ProxyRequire,
        header::Supported,
        header::Unsupported,
        header::Allow,
        header::AllowEvents,
        header::Privacy,
        header::Event,
        header::SubscriptionState,
        header::ReferSub,
        header::MediaType,
        header::AcceptRange,
        header::ContentDisposition,
        header::SessionExpires,
        header::RetryAfter,
        header::Reason,
        header::Replaces,
        header::MaxForwards,
        header::ContentLength,
        header::Expires,
        header::MinExpires,
        header::MinSe,
        header::RSeq,
        header::Authorization,
        header::ProxyAuthorization,
        header::WwwAuthenticate,
        header::ProxyAuthenticate,
        header::CallId,
        header::CSeq,
        header::RAck,
        header::Geolocation,
        header::GeolocationError,
        header::GeolocationRouting,
        header::Via,
        header::ChargingVector,
    );
    Ok(())
}
