//! The option tags this stack understands in a request's `Require` (RFC 3261
//! §8.2.2.3): on any request, the tags it advertises in `Supported`
//! ([`B2BUA_SUPPORTED`]); on a request that negotiates the session, also the
//! tags whose requirement it relays to the peer end to end
//! ([`END_TO_END_OPTION_TAGS`]). A re-INVITE requiring `100rel` is not
//! understood: the stack relays the peer's provisionals to it as they come and
//! puts no requirement to the peer, so none is sure to come reliably (RFC 3262
//! §3). A request requiring a tag not understood, or one the call keeps from
//! the peer that would serve it, is answered 420, its `Unsupported` listing
//! exactly those tags ([`unserved`], [`dropped_on_relay`]).

use crate::generators::B2BUA_SUPPORTED;
use crate::header::{HeaderName, HeaderValue, Require, TokenSet, Unsupported};
use crate::types::SipHeader;
use crate::{Method, SipRequest};

/// The option tag of reliable provisional responses (RFC 3262 §3).
pub const RELIABLE: &str = "100rel";

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

/// The tags of `req`'s `Require` this stack does not understand:
/// [`unserved`] on a call that withholds nothing.
pub fn not_understood(req: &SipRequest) -> Option<Unsupported> {
    unserved(req, &[])
}

/// The tags of `req`'s `Require` this stack cannot serve, in the order the
/// request lists them: those it does not understand, and those in `withheld`,
/// which the call keeps from the peer that would serve them. That is the
/// `Unsupported` set its 420 states. `None` when every tag is served, it
/// requires none, or it is an ACK or a CANCEL, neither of which imposes an
/// extension (§8.2.2.3, §9.1). Tags compare case-insensitively (§7.3.1). A
/// `Require` line that does not read states no tag here: ingress refuses one.
pub fn unserved(req: &SipRequest, withheld: &[String]) -> Option<Unsupported> {
    refused(req, |tag| !understood(tag, req) || withholds(withheld, tag))
}

/// The end-to-end tags `req` requires ([`END_TO_END_OPTION_TAGS`]) that
/// `relayed`, the lines of its copy toward the peer, no longer requires: the
/// copy dropped a requirement nobody else serves (a leg the call keeps out of
/// the session timer, RFC 4028 §7.1). `None` when each rides, or on an ACK or
/// a CANCEL.
pub fn dropped_on_relay(req: &SipRequest, relayed: &[SipHeader]) -> Option<Unsupported> {
    let rides = still_required(relayed);
    refused(req, |tag| is_end_to_end(tag) && !rides.contains(tag))
}

/// [`unserved`] and [`dropped_on_relay`] as one `Unsupported`, in the order
/// `req` lists the tags: what the 420 to a request states once its copy
/// toward the peer, `relayed`, is built.
pub fn unserved_on_relay(
    req: &SipRequest,
    withheld: &[String],
    relayed: &[SipHeader],
) -> Option<Unsupported> {
    let rides = still_required(relayed);
    refused(req, |tag| {
        !understood(tag, req)
            || withholds(withheld, tag)
            || (is_end_to_end(tag) && !rides.contains(tag))
    })
}

/// The end-to-end tags `req` requires ([`END_TO_END_OPTION_TAGS`]): those
/// only its peer serves, so a 2xx this stack writes in the peer's place
/// cannot accept them. `None` when it requires none, or on an ACK or a CANCEL.
pub fn required_end_to_end(req: &SipRequest) -> Option<Unsupported> {
    refused(req, is_end_to_end)
}

/// The tags of `req`'s `Require` that `refuses` picks, in request order;
/// `None` when it picks none, requires nothing, or is an ACK or a CANCEL,
/// neither of which imposes an extension (§8.2.2.3, §9.1).
fn refused(req: &SipRequest, refuses: impl Fn(&str) -> bool) -> Option<Unsupported> {
    if matches!(req.method(), Method::Ack | Method::Cancel) {
        return None;
    }
    let required = req.token_set::<Require>()?.ok()?;
    let refused: Vec<&str> = required.iter().filter(|tag| refuses(tag)).collect();
    (!refused.is_empty()).then(|| Unsupported::of(refused))
}

/// Whether `withheld` names `tag`, case-insensitively (§7.3.1).
fn withholds(withheld: &[String], tag: &str) -> bool {
    withheld.iter().any(|w| w.eq_ignore_ascii_case(tag))
}

/// The tags the `Require` lines among `lines` state, as one set.
fn still_required(lines: &[SipHeader]) -> Require {
    let sets = lines
        .iter()
        .filter(|h| HeaderName::Require.matches(&h.name))
        .filter_map(|h| Require::parse(&h.value).ok());
    Require::union(sets).unwrap_or_else(Require::empty)
}

/// Whether `tag` is one of [`END_TO_END_OPTION_TAGS`].
fn is_end_to_end(tag: &str) -> bool {
    END_TO_END_OPTION_TAGS.iter().any(|e| e.eq_ignore_ascii_case(tag))
}

/// Whether this stack understands `tag` required of it by `req`. A refresh
/// re-INVITE requiring `100rel` draws 420, neither a 408 nor a 481, so the
/// session stands and its refresher may refresh again without the requirement
/// (RFC 4028 §10).
fn understood(tag: &str, req: &SipRequest) -> bool {
    let names = |t: &str| t.eq_ignore_ascii_case(tag);
    let reinvite = *req.method() == Method::Invite && req.to().tag().is_some();
    if reinvite && names(RELIABLE) {
        return false;
    }
    B2BUA_SUPPORTED.split(',').map(crate::whitespace::trim).any(names)
        || (negotiates_session(req.method()) && is_end_to_end(tag))
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

    /// `method` out of any dialog: its To carries no tag.
    fn out_of_dialog(method: &str, require: &str) -> SipRequest {
        let req = request(method, &format!("Require: {require}\r\n"));
        let text = String::from_utf8(req.image().to_vec()).expect("the request is text");
        match CustomParser::new().parse(text.replace(";tag=b1", "").as_bytes()) {
            Ok(SipMessage::Request(req)) => req,
            other => unreachable!("a request was written: {other:?}"),
        }
    }

    #[test]
    fn every_advertised_tag_is_understood_on_any_request_but_a_reinvite() {
        for method in ["INFO", "BYE", "MESSAGE", "NOTIFY", "REFER", "PRACK", "UPDATE"] {
            assert_eq!(refused(method, "100rel, replaces"), None, "{method}");
        }
        let initial = out_of_dialog("INVITE", "100rel, replaces");
        assert!(not_understood(&initial).is_none(), "an initial INVITE");
    }

    #[test]
    fn a_reinvite_requiring_100rel_is_refused_it() {
        assert_eq!(refused("INVITE", "replaces, 100rel").as_deref(), Some("100rel"));
    }

    #[test]
    fn a_withheld_tag_is_unserved_beside_the_tags_not_understood_in_request_order() {
        let req = request("UPDATE", "Require: x-b, TIMER, replaces\r\n");
        let unserved = unserved(&req, &["timer".to_string()]).map(|u| u.to_wire());
        assert_eq!(unserved.as_deref(), Some("x-b, TIMER"));
        assert!(super::unserved(&request("INFO", ""), &["timer".to_string()]).is_none());
    }

    fn line(name: &str, value: &str) -> SipHeader {
        SipHeader { name: crate::SipStr::owned(name), value: crate::SipStr::owned(value) }
    }

    #[test]
    fn an_end_to_end_requirement_the_copy_lost_is_dropped_on_relay() {
        let req = request("UPDATE", "Require: timer, replaces\r\n");
        let dropped = |relayed: &[SipHeader]| dropped_on_relay(&req, relayed).map(|u| u.to_wire());
        assert_eq!(dropped(&[]).as_deref(), Some("timer"), "no Require rides");
        assert_eq!(dropped(&[line("Supported", "timer")]).as_deref(), Some("timer"));
        assert_eq!(dropped(&[line("Require", "Timer")]), None, "it rides");
        assert_eq!(dropped_on_relay(&request("UPDATE", ""), &[]), None, "nothing required");
    }

    #[test]
    fn a_built_copy_adds_its_dropped_tags_to_the_unserved_in_request_order() {
        let req = out_of_dialog("INVITE", "x-b, timer, 100rel");
        let withheld = ["100rel".to_string()];
        let on_relay = |relayed: &[SipHeader]| {
            unserved_on_relay(&req, &withheld, relayed).map(|u| u.to_wire())
        };
        assert_eq!(on_relay(&[]).as_deref(), Some("x-b, timer, 100rel"));
        assert_eq!(on_relay(&[line("Require", "timer")]).as_deref(), Some("x-b, 100rel"));
    }

    #[test]
    fn only_the_end_to_end_tags_are_required_end_to_end() {
        let req = request("UPDATE", "Require: 100rel, Timer\r\n");
        assert_eq!(required_end_to_end(&req).map(|u| u.to_wire()).as_deref(), Some("Timer"));
        assert!(required_end_to_end(&request("UPDATE", "Require: 100rel\r\n")).is_none());
        assert!(required_end_to_end(&request("ACK", "Require: timer\r\n")).is_none());
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
