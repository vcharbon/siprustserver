//! UAS response generation: echo Via / From / To / Call-ID / CSeq from the
//! request being answered (RFC 3261 §8.2.6.2). Rebuilding a response from
//! B2BUA-snapshotted fields lives in [`super::relay`].

use super::emit;
use crate::draft::ResponseDraft;
use crate::header::{self, HeaderName, HeaderValue, MediaType, To, Via};
use crate::sip_str::SipStr;
use crate::types::{SipHeader, SipRequest, SipResponse};

/// Deterministic fallback To-tag for a non-100 response whose request carried a
/// tag-less To and whose caller supplied no `to_tag`. Derived from the Call-ID so
/// it is stable per call (a retransmit re-derives the same tag) and unique across
/// calls. This only fires on a degenerate path — it exists so the worker emits a
/// well-formed response instead of panicking. See [`generate_response`].
fn fallback_to_tag(call_id: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    call_id.hash(&mut hasher);
    format!("b2bua-fb-{:016x}", hasher.finish())
}

/// Inputs for [`generate_response`]. Via / From / To / Call-ID / CSeq are not
/// inputs at all — §8.2.6.2 makes them echoes of the request, carried verbatim.
#[derive(Debug, Clone, Default)]
pub struct GenerateResponseOpts {
    /// Tag added to To when status > 100 and the request's To lacks one.
    pub to_tag: Option<String>,
    pub contact: Option<header::Contact>,
    pub body: Vec<u8>,
    pub content_type: Option<MediaType>,
    /// Caller-stated header lines, carried verbatim (name spelling included).
    pub extra_headers: Vec<SipHeader>,
    /// Source the request arrived from — stamps `received=` / `rport=` on the
    /// topmost echoed Via (RFC 3261 §18.2.1 + RFC 3581 §4).
    pub incoming_source: Option<(String, u16)>,
}

/// Echo one Via line. The topmost records what the receiving side observed
/// (RFC 3261 §18.2.1, RFC 3581 §4); a line needing no stamp — or folding
/// several hops onto one line — is echoed byte for byte.
fn echo_via(draft: ResponseDraft, line: SipStr, source: Option<&(String, u16)>) -> ResponseDraft {
    let Some((host, port)) = source else {
        return draft.push_raw(HeaderName::Via, line);
    };
    let single = Via::parse_line(&line).ok().filter(|hops| hops.len() == 1);
    match single.map(|mut hops| hops.remove(0)) {
        Some(hop) => {
            let stamped = hop.clone().stamped_from(host, *port);
            if stamped == hop {
                draft.push_raw(HeaderName::Via, line)
            } else {
                draft.push(stamped)
            }
        }
        None => draft.push_raw(HeaderName::Via, line),
    }
}

/// Echo the request's To, tagged as the dialog requires. A non-100 response
/// MUST carry a To-tag (RFC 3261 §8.2.6.2): a To that already has one (in
/// dialog) is echoed, otherwise `to_tag` is added when supplied, else a
/// deterministic fallback — a worker must never panic building a response
/// (that kills the handler task and leaks the dialog).
fn echo_to(
    draft: ResponseDraft,
    line: SipStr,
    status: u16,
    call_id: &str,
    to_tag: Option<&str>,
) -> ResponseDraft {
    if status <= 100 {
        return draft.push_raw(HeaderName::To, line);
    }
    let tag = || to_tag.map(str::to_owned).unwrap_or_else(|| fallback_to_tag(call_id));
    match To::parse(&line) {
        Ok(to) if to.tag().is_some() => draft.push_raw(HeaderName::To, line),
        Ok(to) => draft.push(to.with_tag(SipStr::owned(&tag()))),
        // A To the strict reader cannot read still leaves tagged, so the peer
        // rejects the address it sent rather than a tag this stack dropped.
        Err(_) => draft
            .push_raw(HeaderName::To, SipStr::owned(&format!("{};tag={}", line.as_str(), tag()))),
    }
}

/// Build a UAS response to `incoming_request`, echoing Via / From / To /
/// Call-ID / CSeq (RFC 3261 §8.2.6.2).
pub fn generate_response(
    incoming_request: &SipRequest,
    status: u16,
    reason: &str,
    opts: &GenerateResponseOpts,
) -> SipResponse {
    let line = |name: HeaderName| incoming_request.raw_text(name).next().unwrap_or(SipStr::EMPTY);
    let call_id = line(HeaderName::CallId);

    let mut draft = ResponseDraft::new(status, SipStr::owned(reason));
    for (i, via) in incoming_request.raw_text(HeaderName::Via).enumerate() {
        let source = (i == 0).then_some(opts.incoming_source.as_ref()).flatten();
        draft = echo_via(draft, via, source);
    }

    // Echo Record-Route verbatim (RFC 3261 §16.6) — but NOT on a 100 Trying: a
    // 100 establishes no dialog, so its Record-Route is inert (the UAC ignores it)
    // and merely bloats the provisional. Dialog-establishing 18x/2xx still carry it.
    if status != 100 {
        for record_route in incoming_request.raw_text(HeaderName::RecordRoute) {
            draft = draft.push_raw(HeaderName::RecordRoute, record_route);
        }
    }

    draft = draft.push_raw(HeaderName::From, line(HeaderName::From));
    draft = echo_to(draft, line(HeaderName::To), status, call_id.as_str(), opts.to_tag.as_deref());
    draft = draft
        .push_raw(HeaderName::CallId, call_id)
        .push_raw(HeaderName::CSeq, line(HeaderName::CSeq));

    // RFC 3261 §8.2.6.1 / §20.38: a request carrying a Timestamp is answered
    // with that same Timestamp, so the requester can measure the round trip
    // against the value it sent. The delay this stack adds is not measured, so
    // no delay is appended. A caller stating its own Timestamp owns it.
    if !emit::carries(&opts.extra_headers, &HeaderName::Timestamp) {
        for timestamp in incoming_request.raw_text(HeaderName::Timestamp).take(1) {
            draft = draft.push_raw(HeaderName::Timestamp, timestamp);
        }
    }

    if let Some(contact) = opts.contact.clone() {
        draft = draft.push(contact);
    }

    // RFC 3261 §8.2.1: a 405 names the methods this stack accepts (`Allow`),
    // so the requester can pick another instead of retrying into silence. A
    // caller stating its own Allow owns it.
    if status == 405 && !emit::carries(&opts.extra_headers, &HeaderName::Allow) {
        draft = draft.push_raw(HeaderName::Allow, SipStr::owned(super::B2BUA_ALLOW));
    }

    draft = emit::extra_headers(draft, &opts.extra_headers);
    emit::response(emit::framed(draft, opts.body.clone(), opts.content_type.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::custom::CustomParser;
    use crate::SipParser;

    fn notify() -> SipRequest {
        let raw = "NOTIFY sip:as@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 127.0.0.1:5070;branch=z9hG4bK-n1\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@127.0.0.1:5070>;tag=a1\r\n\
To: <sip:as@127.0.0.1:5060>;tag=b1\r\n\
Call-ID: allow-405\r\n\
CSeq: 2 NOTIFY\r\n\
Content-Length: 0\r\n\r\n";
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            crate::SipMessage::Request(r) => r,
            _ => panic!("expected request"),
        }
    }

    /// RFC 3261 §8.2.1 — a locally minted 405 names the accepted methods.
    #[test]
    fn a_405_carries_the_stack_allow_set() {
        let resp =
            generate_response(&notify(), 405, "Method Not Allowed", &Default::default());
        let allow: Vec<_> = resp.raw_text(HeaderName::Allow).collect();
        assert_eq!(allow.len(), 1);
        assert_eq!(allow[0].as_str(), super::super::B2BUA_ALLOW);
    }

    /// A caller stating its own Allow owns the value — no second line.
    #[test]
    fn a_caller_stated_allow_owns_the_405() {
        let opts = GenerateResponseOpts {
            extra_headers: vec![SipHeader {
                name: SipStr::owned("Allow"),
                value: SipStr::owned("INVITE, ACK, BYE"),
            }],
            ..Default::default()
        };
        let resp = generate_response(&notify(), 405, "Method Not Allowed", &opts);
        let allow: Vec<_> = resp.raw_text(HeaderName::Allow).collect();
        assert_eq!(allow.len(), 1);
        assert_eq!(allow[0].as_str(), "INVITE, ACK, BYE");
    }

    /// The stamp is the 405's alone — other statuses state Allow only where
    /// their own mint points do.
    #[test]
    fn a_200_gains_no_allow_here() {
        let resp = generate_response(&notify(), 200, "OK", &Default::default());
        assert_eq!(resp.raw_text(HeaderName::Allow).count(), 0);
    }
}
