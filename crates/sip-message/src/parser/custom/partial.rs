//! What a refused request still states correctly enough to be answered: the
//! request line's method and the five header fields a response echoes
//! (RFC 3261 §8.2.6.2), so a refused request can draw a 400 (§8.2.6, §16.3).

use super::extract_fields::{extract_identity, ExtractMode, Identity};
use super::header_index::HeaderIndex;
use crate::header::{self, Via};
use crate::method::Method;
use crate::parser::SipParserLimits;
use crate::sip_str::SipStr;
use crate::types::NonEmpty;

/// A refused request's method and its Via, From, To, Call-ID and CSeq: typed,
/// each having passed the parse's full rules, and as their lines read on the
/// wire, which a response echoes byte for byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestPartial {
    method: Method,
    identity: Identity,
    via_lines: Vec<SipStr>,
    from_line: SipStr,
    to_line: SipStr,
    call_id_line: SipStr,
    cseq_line: SipStr,
}

impl RequestPartial {
    /// The method of the request line.
    pub fn method(&self) -> &Method {
        &self.method
    }

    pub fn via(&self) -> &NonEmpty<Via> {
        &self.identity.via
    }

    pub fn from(&self) -> &header::From {
        &self.identity.from
    }

    pub fn to(&self) -> &header::To {
        &self.identity.to
    }

    pub fn call_id(&self) -> &header::CallId {
        &self.identity.call_id
    }

    pub fn cseq(&self) -> &header::CSeq {
        &self.identity.cseq
    }

    /// The Via lines as received, top first.
    pub fn via_lines(&self) -> &[SipStr] {
        &self.via_lines
    }

    /// The From line as received.
    pub fn from_line(&self) -> &SipStr {
        &self.from_line
    }

    /// The To line as received.
    pub fn to_line(&self) -> &SipStr {
        &self.to_line
    }

    /// The Call-ID line as received.
    pub fn call_id_line(&self) -> &SipStr {
        &self.call_id_line
    }

    /// The CSeq line as received.
    pub fn cseq_line(&self) -> &SipStr {
        &self.cseq_line
    }
}

/// The partial of a refused request with method `method` whose header lines
/// `idx` indexes. `None` when no response can answer it: an ACK (§17), or a
/// request one of whose five fields does not pass `mode`'s rules.
pub(super) fn request_partial(
    method: &str,
    idx: &HeaderIndex,
    limits: &SipParserLimits,
    mode: ExtractMode,
) -> Option<Box<RequestPartial>> {
    let method = Method::from_wire(method);
    if method == Method::Ack {
        return None;
    }
    let identity = extract_identity(idx, limits, mode).ok()?;
    Some(Box::new(RequestPartial {
        method,
        identity,
        via_lines: idx.via.iter().map(|line| (*line).clone()).collect(),
        from_line: idx.from.first?.clone(),
        to_line: idx.to.first?.clone(),
        call_id_line: idx.call_id.first?.clone(),
        cseq_line: idx.cseq.first?.clone(),
    }))
}
