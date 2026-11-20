//! The caller's INVITE, rebuilt from the call's snapshot of it by the step
//! that reads it (ADR-0052 §11).

use sip_message::draft::{IncompleteDraft, RequestDraft};
use sip_message::header::{HeaderName, HeaderValue, MaxForwards, MediaType, Uri};
use sip_message::{Method, SipRequest, SipStr};

use crate::model::{RuleContext, RuleHandleResult};

/// Rebuild the a-leg's original INVITE as a `SipRequest`. Every header rides
/// as an unparsed line, so the rebuilt message carries the caller's bytes
/// exactly as they arrived. A caller that omitted `Max-Forwards` gets RFC 3261
/// §8.1.1.6's default, which the transfer leg originated from this rebuild
/// then carries.
///
/// The Request-URI is read verbatim, not refused: this is a round-trip of text
/// the inbound parser's own strict gates already admitted. Nothing routes on
/// it — the b-leg's Request-URI comes from the decision.
pub fn rebuild(snap: &call::ALegInviteSnapshot) -> Result<SipRequest, IncompleteDraft> {
    let mut draft =
        RequestDraft::new(Method::Invite, Uri::parse_or_verbatim(&SipStr::owned(&snap.uri)));
    for h in &snap.headers {
        draft = draft.push_raw(HeaderName::from(h.name.as_str()), SipStr::owned(&h.value));
    }
    if !draft.has(&HeaderName::MaxForwards) {
        draft = draft.set(MaxForwards::DEFAULT);
    }
    draft.with_body(snap.body.clone().into()).freeze()
}

/// The session description the caller's INVITE carries, bare or framed in a
/// `multipart/…` body (RFC 5621 §3.1), as [`sip_message::sdp_range`] reads it
/// from her `Content-Type` and `Content-Disposition`. `None` for an INVITE
/// that makes no offer (RFC 3261 §13.2.1), whatever other body it carries.
pub fn description(snap: &call::ALegInviteSnapshot) -> Option<&[u8]> {
    let first = |name: HeaderName| {
        snap.headers.iter().find(|h| name.matches(&h.name)).map(|h| h.value.as_str())
    };
    let disposition = first(HeaderName::ContentDisposition);
    let content_type = MediaType::parse(&SipStr::owned(first(HeaderName::ContentType)?)).ok()?;
    snap.body.get(sip_message::sdp_range(&content_type, disposition, &snap.body)?)
}

impl RuleContext<'_> {
    /// The caller's INVITE, rebuilt from the call's record. `None` is a record
    /// that does not rebuild it, which the rule answers with
    /// [`RuleHandleResult::unrecoverable`].
    pub fn caller_invite(&self) -> Option<SipRequest> {
        rebuild(self.call.a_leg_invite()).ok()
    }
}

impl RuleHandleResult {
    /// What a rule returns when the call's record does not rebuild the
    /// caller's INVITE it reads: no action, and the engine takes the call as
    /// unrecoverable.
    pub fn unrecoverable() -> Self {
        Self { unrecoverable: true, ..Self::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::description;

    const SDP: &str =
        "v=0\r\no=a 1 1 IN IP4 192.0.2.1\r\ns=-\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\n";

    fn invite(content_type: Option<&str>, body: &str) -> call::ALegInviteSnapshot {
        let line = |v: &str| call::SipHeader { name: "Content-Type".into(), value: v.into() };
        call::ALegInviteSnapshot {
            uri: "sip:bob@example.com".into(),
            headers: content_type.map(line).into_iter().collect(),
            body: body.as_bytes().to_vec(),
            cseq: 1,
            from_tag: "a".into(),
        }
    }

    fn framed(parts: &[(&str, &str)]) -> String {
        let mut body = String::new();
        for (content_type, text) in parts {
            body.push_str(&format!("--B\r\nContent-Type: {content_type}\r\n\r\n{text}\r\n"));
        }
        body + "--B--\r\n"
    }

    #[test]
    fn a_bare_description_is_the_whole_body() {
        let snap = invite(Some("application/sdp"), SDP);
        assert_eq!(description(&snap), Some(SDP.as_bytes()));
    }

    #[test]
    fn a_framed_description_is_its_part_alone() {
        let body = framed(&[("application/sdp", SDP), ("text/plain", "a note")]);
        let snap = invite(Some("multipart/mixed;boundary=B"), &body);
        assert_eq!(description(&snap), Some(SDP.as_bytes()));
    }

    /// A body of other parts only makes no offer (RFC 3261 §13.2.1).
    #[test]
    fn parts_without_a_description_make_no_offer() {
        let body = framed(&[("text/plain", "a note"), ("application/pidf+xml", "<presence/>")]);
        let snap = invite(Some("multipart/mixed;boundary=B"), &body);
        assert_eq!(description(&snap), None);
        assert_eq!(description(&invite(Some("text/plain"), "a note")), None);
        assert_eq!(description(&invite(None, "")), None);
    }
}
