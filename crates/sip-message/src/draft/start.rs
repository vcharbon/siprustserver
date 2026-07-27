//! The start line — the only thing a request draft and a response draft do not
//! share.
//!
//! [`StartKind`] contributes the line's type, the header set `freeze` requires,
//! and the assembly of the frozen message. Everything else in the draft engine
//! is written once.

use bytes::Bytes;

use crate::error::SipParseError;
use crate::header::{HeaderName, Uri, Wire};
use crate::method::Method;
use crate::parser::custom::extract_fields::{
    extract_request_fields, extract_response_fields, ExtractMode,
};
use crate::parser::custom::header_index::HeaderIndex;
use crate::parser::custom::optional_headers::extract_optional_indexed;
use crate::parser::SipParserLimits;
use crate::sip_str::{SharedText, SipStr};
use crate::types::{MessageCore, SipHeader, SipRequest, SipResponse};

/// A byte range of the rendered datagram.
pub type Span = (usize, usize);

/// Where the start line put the text a frozen message keeps as a field.
pub struct StartSpans {
    /// The Request-URI of a request, the reason phrase of a response.
    pub subject: Span,
    pub version: Span,
}

/// What a draft direction contributes.
pub trait StartKind: 'static {
    /// The start line under construction.
    type Line: Clone + std::fmt::Debug + Send + Sync;
    /// The frozen message this direction produces.
    type Message;

    /// The headers RFC 3261 requires on every message of this direction
    /// (§8.1.1 for requests, §7.2 for responses).
    const REQUIRED: &'static [HeaderName];

    fn render_start(line: &Self::Line, out: &mut Wire) -> StartSpans;

    fn assemble(
        line: Self::Line,
        image: &SharedText,
        spans: StartSpans,
        headers: Vec<SipHeader>,
        body: Bytes,
        raw: Bytes,
    ) -> Result<Self::Message, SipParseError>;
}

/// The direction markers. Named `kind` to mirror the header model's kind axis.
pub mod kind {
    /// A request under construction.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Request;
    /// A response under construction.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Response;
}

/// `Method SP Request-URI SP SIP-Version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestLine {
    pub method: Method,
    pub uri: Uri,
    pub version: SipStr,
}

/// `SIP-Version SP Status-Code SP Reason-Phrase`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    pub version: SipStr,
    pub status: u16,
    pub reason: SipStr,
}

/// The version every message this stack emits carries.
pub const SIP_VERSION: &str = "SIP/2.0";

impl StartKind for kind::Request {
    type Line = RequestLine;
    type Message = SipRequest;

    const REQUIRED: &'static [HeaderName] = &[
        HeaderName::Via,
        HeaderName::From,
        HeaderName::To,
        HeaderName::CallId,
        HeaderName::CSeq,
        HeaderName::MaxForwards,
    ];

    fn render_start(line: &Self::Line, out: &mut Wire) -> StartSpans {
        out.str(line.method.as_str());
        out.byte(b' ');
        let uri_at = out.len();
        line.uri.render(out);
        let subject = (uri_at, out.len() - uri_at);
        out.byte(b' ');
        let version_at = out.len();
        out.str(line.version.as_str());
        StartSpans { subject, version: (version_at, out.len() - version_at) }
    }

    fn assemble(
        line: Self::Line,
        image: &SharedText,
        spans: StartSpans,
        headers: Vec<SipHeader>,
        body: Bytes,
        raw: Bytes,
    ) -> Result<Self::Message, SipParseError> {
        let limits = SipParserLimits::default();
        let uri = image.span(spans.subject.0, spans.subject.1);
        let idx = HeaderIndex::build(&headers);
        let eager = extract_request_fields(
            &idx,
            &uri,
            &limits,
            Some(line.method.as_str()),
            ExtractMode::Hydrate,
        )?;
        let optional = extract_optional_indexed(&idx);
        Ok(SipRequest::new(
            RequestLine {
                method: line.method,
                uri: line.uri,
                version: image.span(spans.version.0, spans.version.1),
            },
            MessageCore::new(headers, eager.common, optional, body, raw),
        ))
    }
}

impl StartKind for kind::Response {
    type Line = StatusLine;
    type Message = SipResponse;

    const REQUIRED: &'static [HeaderName] = &[
        HeaderName::Via,
        HeaderName::From,
        HeaderName::To,
        HeaderName::CallId,
        HeaderName::CSeq,
    ];

    fn render_start(line: &Self::Line, out: &mut Wire) -> StartSpans {
        let version_at = out.len();
        out.str(line.version.as_str());
        let version = (version_at, out.len() - version_at);
        out.byte(b' ');
        out.num(line.status as u64);
        out.byte(b' ');
        let reason_at = out.len();
        out.str(line.reason.as_str());
        StartSpans { subject: (reason_at, out.len() - reason_at), version }
    }

    fn assemble(
        line: Self::Line,
        image: &SharedText,
        spans: StartSpans,
        headers: Vec<SipHeader>,
        body: Bytes,
        raw: Bytes,
    ) -> Result<Self::Message, SipParseError> {
        let limits = SipParserLimits::default();
        let idx = HeaderIndex::build(&headers);
        let core = extract_response_fields(&idx, line.status, &limits, ExtractMode::Hydrate)?;
        let optional = extract_optional_indexed(&idx);
        Ok(SipResponse::new(
            StatusLine {
                version: image.span(spans.version.0, spans.version.1),
                status: line.status,
                reason: image.span(spans.subject.0, spans.subject.1),
            },
            MessageCore::new(headers, core, optional, body, raw),
        ))
    }
}
