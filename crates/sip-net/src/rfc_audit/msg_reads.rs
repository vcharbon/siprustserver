//! Generic accessors over a parsed recorded message — the small reads the live
//! adapter needs to build a `rfc_rules::Msg` and to label a finding.
//!
//! Rule logic does NOT live here: an obligation is a body in `rfc-rules`. SIP
//! grammar does not either — every read below delegates to `sip_message`.

use sip_message::SipMessage;

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
