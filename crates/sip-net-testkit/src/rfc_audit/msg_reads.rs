//! Generic accessors over a parsed recorded message — the small reads the live
//! adapter builds a `rfc_rules::Msg` from and labels a finding with, and the
//! [`CaptureIndex`](super::capture_index::CaptureIndex) reads a capture with.
//!
//! Rule logic does NOT live here: an obligation is a body in `rfc-rules`. SIP
//! grammar does not either — every read below delegates to `sip_message`.

use sip_message::header::{RAck, RSeq};
use sip_message::{Method, SipMessage};

/// `From` tag, in either direction (the originator's tag rides From on requests
/// and on the responses to them).
pub fn from_tag(m: &SipMessage) -> Option<&str> {
    match m {
        SipMessage::Request(r) => r.from().tag(),
        SipMessage::Response(r) => r.from().tag(),
    }
}

/// `To` tag (the answerer's dialog tag; absent on the dialog-creating request
/// and on 100 Trying).
pub fn to_tag(m: &SipMessage) -> Option<&str> {
    match m {
        SipMessage::Request(r) => r.to().tag(),
        SipMessage::Response(r) => r.to().tag(),
    }
}

pub fn call_id(m: &SipMessage) -> &str {
    m.call_id().as_str()
}

/// A request's method; `None` for a response.
pub fn request_method(m: &SipMessage) -> Option<&Method> {
    match m {
        SipMessage::Request(r) => Some(r.method()),
        SipMessage::Response(_) => None,
    }
}

/// CSeq method token (`INVITE`, `BYE`, …).
pub fn cseq_method(m: &SipMessage) -> &str {
    match m {
        SipMessage::Request(r) => r.cseq().method().as_str(),
        SipMessage::Response(r) => r.cseq().method().as_str(),
    }
}

pub fn cseq_seq(m: &SipMessage) -> u32 {
    match m {
        SipMessage::Request(r) => r.cseq().seq(),
        SipMessage::Response(r) => r.cseq().seq(),
    }
}

/// Response status, or `0` for a request.
pub fn status(m: &SipMessage) -> u16 {
    match m {
        SipMessage::Request(_) => 0,
        SipMessage::Response(r) => r.status(),
    }
}

/// The top (first) `Via` `branch=` token, if present and non-empty.
pub fn top_via_branch(m: &SipMessage) -> Option<String> {
    m.top_via().branch().filter(|b| !b.is_empty()).map(str::to_string)
}

/// A response's `RSeq` (RFC 3262 §7.1); `None` for a request, or where it is
/// absent or unreadable.
pub fn rseq(m: &SipMessage) -> Option<u32> {
    match m {
        SipMessage::Request(_) => None,
        SipMessage::Response(r) => r.header::<RSeq>().and_then(Result::ok).map(|r| r.value()),
    }
}

/// A request's `RAck` (RFC 3262 §7.2): the RSeq and the CSeq number it
/// acknowledges; `None` for a response, or where it is absent or unreadable.
pub fn rack(m: &SipMessage) -> Option<(u32, u32)> {
    match m {
        SipMessage::Request(r) => {
            r.header::<RAck>().and_then(Result::ok).map(|r| (r.rseq(), r.seq()))
        }
        SipMessage::Response(_) => None,
    }
}
