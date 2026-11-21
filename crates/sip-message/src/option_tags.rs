//! The option tags this stack understands in a request's `Require` (RFC 3261
//! §8.2.2.3): on any request, the tags it advertises in `Supported`
//! ([`B2BUA_SUPPORTED`]); on a request that negotiates the session, also the
//! tags whose requirement it relays to the peer end to end
//! ([`END_TO_END_OPTION_TAGS`]). A request requiring any other tag is
//! answered 420, its `Unsupported` listing exactly those tags
//! ([`not_understood`]).

use crate::generators::B2BUA_SUPPORTED;
use crate::header::{Require, Unsupported};
use crate::{Method, SipRequest};

/// The option tags of a session request's `Require` that ride to the peer:
/// extensions whose headers and messages this stack relays end to end without
/// taking part, so the peer is the UAS the requirement is put to. RFC 4028
/// §7.1: `timer` asks the UAS to support the session timer, whose negotiation
/// and refreshes cross the back-to-back UA unchanged. On a request that
/// negotiates no session, such a tag names nothing this stack carries and is
/// not understood.
pub const END_TO_END_OPTION_TAGS: &[&str] = &["timer"];

/// True iff `method` negotiates the session, so a requirement on its UAS
/// concerns the session's extensions: the INVITE and the two requests that
/// refresh it (RFC 4028 §7.4: re-INVITE, UPDATE).
pub fn negotiates_session(method: &Method) -> bool {
    matches!(method, Method::Invite | Method::Update)
}

/// The tags of `req`'s `Require` this stack does not understand, in the order
/// the request lists them: the `Unsupported` set its 420 states. `None` when
/// it understands every tag, requires none, or is an ACK or a CANCEL, neither
/// of which imposes an extension (§8.2.2.3, §9.1). Tags compare
/// case-insensitively (§7.3.1). A `Require` line that does not read states no
/// tag here: ingress refuses one.
pub fn not_understood(req: &SipRequest) -> Option<Unsupported> {
    if matches!(req.method(), Method::Ack | Method::Cancel) {
        return None;
    }
    let required = req.token_set::<Require>()?.ok()?;
    let session = negotiates_session(req.method());
    let unknown: Vec<&str> = required.iter().filter(|tag| !understood(tag, session)).collect();
    (!unknown.is_empty()).then(|| Unsupported::of(unknown))
}

/// Whether this stack understands `tag` required of it, on a request that
/// negotiates the session where `session`.
fn understood(tag: &str, session: bool) -> bool {
    let names = |t: &str| t.eq_ignore_ascii_case(tag);
    B2BUA_SUPPORTED.split(',').map(crate::whitespace::trim).any(names)
        || (session && END_TO_END_OPTION_TAGS.iter().copied().any(names))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::HeaderValue;
    use crate::parser::custom::CustomParser;
    use crate::{SipMessage, SipParser};

    fn request(method: &str, require: &str) -> SipRequest {
        let text = format!(
            "{method} sip:bob@192.0.2.2 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-1\r\n\
Max-Forwards: 70\r\n\
Contact: <sip:alice@192.0.2.1>\r\n\
From: <sip:alice@192.0.2.1>;tag=a1\r\n\
To: <sip:bob@192.0.2.2>;tag=b1\r\n\
Call-ID: c1@192.0.2.1\r\n\
CSeq: 2 {method}\r\n\
{require}Content-Length: 0\r\n\r\n"
        );
        match CustomParser::new().parse(text.as_bytes()).expect("the request parses") {
            SipMessage::Request(req) => req,
            SipMessage::Response(_) => unreachable!("a request was written"),
        }
    }

    fn refused(method: &str, require: &str) -> Option<String> {
        not_understood(&request(method, &format!("Require: {require}\r\n"))).map(|u| u.to_wire())
    }

    #[test]
    fn every_advertised_tag_is_understood_on_any_request() {
        for method in ["INFO", "BYE", "INVITE", "MESSAGE", "NOTIFY", "REFER", "PRACK"] {
            assert_eq!(refused(method, "100rel, replaces"), None, "{method}");
        }
    }

    #[test]
    fn timer_is_understood_only_on_a_request_that_negotiates_the_session() {
        assert_eq!(refused("INVITE", "timer"), None);
        assert_eq!(refused("UPDATE", "Timer"), None);
        assert_eq!(refused("INFO", "timer").as_deref(), Some("timer"));
        assert_eq!(refused("BYE", "timer").as_deref(), Some("timer"));
    }

    #[test]
    fn only_the_tags_not_understood_are_listed_in_request_order() {
        assert_eq!(
            refused("UPDATE", "x-b, 100REL, timer, x-a").as_deref(),
            Some("x-b, x-a"),
            "100rel and timer are understood on an UPDATE"
        );
    }

    #[test]
    fn several_require_lines_are_one_set() {
        let req = request("INFO", "Require: 100rel\r\nRequire: x-one\r\n");
        assert_eq!(not_understood(&req).map(|u| u.to_wire()).as_deref(), Some("x-one"));
    }

    #[test]
    fn a_request_requiring_nothing_is_refused_nothing() {
        assert!(not_understood(&request("INFO", "")).is_none());
    }

    #[test]
    fn an_ack_or_a_cancel_is_never_refused() {
        assert_eq!(refused("ACK", "x-nonexistent"), None);
        assert_eq!(refused("CANCEL", "x-nonexistent"), None);
    }
}
