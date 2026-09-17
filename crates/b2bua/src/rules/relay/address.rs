//! Decision-supplied address reading and its refusal. Every address a decision
//! names on the routing path (`new_ruri`, `new_from`, `new_to`, a redirect
//! `contact`) goes through one reader; text no reader accepts becomes an
//! [`UnreadableAddress`] refusal, never a fabricated target. Dialog-state text
//! readback does NOT live here — see [`super::dialog`].

use sip_message::header::{self, HeaderName, HeaderValue, NameAddr, ParamValue, Uri};
use sip_message::{SipHeader as MsgHeader, SipStr};

/// An address a decision named that no reader accepts, so the B2BUA has nothing
/// to route toward. `Display` names the offending **field only** — it is what a
/// refusal puts in a reason phrase, and the field is this stack's own static
/// text, so nothing a peer wrote can reach the wire through it. The value and
/// the reader's reason ride [`detail`](Self::detail), for the local log.
///
/// The B2BUA never answers such a field by inventing a value:
/// an opaque URI's host is the whole raw text, so originating on one dials an
/// address nobody named — the failure rides this error to a seam that can 5xx
/// the affected leg and write the CDR.
#[derive(Debug, Clone)]
pub struct UnreadableAddress {
    /// The decision field the text came from (`new_ruri`, `contact`, …).
    pub field: &'static str,
    /// The text as the decision stated it.
    pub value: String,
    /// Why no reader accepts it.
    pub reason: String,
}

impl std::fmt::Display for UnreadableAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Unreadable Routing Address ({})", self.field)
    }
}

impl std::error::Error for UnreadableAddress {}

impl UnreadableAddress {
    /// The full detail, value included — for a local log, never for the wire.
    pub fn detail(&self) -> String {
        format!("{}={:?}: {}", self.field, self.value, self.reason)
    }
}

/// The address `text` names, or a refusal. The one reader every decision-
/// supplied address on the routing path goes through.
pub(super) fn address(field: &'static str, text: &str) -> Result<Uri, UnreadableAddress> {
    Uri::parse(&SipStr::owned(text)).map_err(|err| UnreadableAddress {
        field,
        value: text.to_string(),
        reason: err.reason,
    })
}

/// The identity `text` names on a From or To a decision states, read by
/// [`NameAddr::parse_identity`], or a refusal. The core owns the dialog tags
/// (ADR-0017 X2): a `tag` header or URI parameter is dropped; a `tag=` inside the
/// userinfo or a `?`-header value is refused, the freeze counting it as a second tag.
pub(super) fn identity(field: &'static str, text: &str) -> Result<NameAddr, UnreadableAddress> {
    let refuse = |reason: String| UnreadableAddress { field, value: text.to_string(), reason };
    let addr = NameAddr::parse_identity(&SipStr::owned(text)).map_err(|err| refuse(err.reason))?;
    let uri = addr.uri();
    if uri.user().is_some_and(|u| u.to_ascii_lowercase().contains("tag=")) {
        return Err(refuse("a tag in the userinfo".to_string()));
    }
    if uri
        .escaped_headers()
        .any(|(_, v)| v.is_some_and(|v| v.to_ascii_lowercase().contains("tag=")))
    {
        return Err(refuse("a tag in a URI header".to_string()));
    }
    let uri = addr.uri().clone().without_param("tag");
    Ok(addr.without_param("tag").with_uri(uri))
}

/// One `Contact: <uri>;q=…` redirect target (RFC 3261 §20.10) for a 3xx the
/// B2BUA authors. A target no reader accepts is refused: the caller dials what
/// a 3xx Contact names, so an invented one sends it at an address the decision
/// never stated.
pub fn redirect_contact(uri: &str, q: Option<f32>) -> Result<MsgHeader, UnreadableAddress> {
    let mut contact = header::Contact::new(NameAddr::new(address("contact", uri)?));
    if let Some(q) = q {
        contact = contact.with_param("q", ParamValue::text(SipStr::owned(&q.to_string())));
    }
    Ok(MsgHeader {
        name: SipStr::owned(HeaderName::Contact.as_wire_str()),
        value: SipStr::owned(&contact.to_wire()),
    })
}

#[cfg(test)]
mod unreadable_address_tests {
    //! A decision-supplied address that no reader accepts is a refusal, never
    //! a fabricated value: an opaque URI's host would be the whole raw text,
    //! so originating on one dials an address nobody named — or dies later as
    //! an unattributable transport error.
    use super::{redirect_contact, UnreadableAddress};
    use crate::config::B2buaConfig;
    use crate::effects::{OutboundBody, OutboundSipEffect};
    use crate::rules::relay::build_b_leg;
    use call::Leg;
    use sip_message::generators::CapabilitySet;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser, SipRequest};
    use sip_txn::IdGen;

    fn a_leg_invite() -> SipRequest {
        let raw = "INVITE sip:bob@10.244.2.7:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-alice;lg=a\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@192.0.2.5:5060>;tag=alice-from-tag\r\n\
To: <sip:bob@10.244.2.7:5060>\r\n\
Call-ID: alice-call-id@192.0.2.5\r\n\
CSeq: 314 INVITE\r\n\
Content-Length: 0\r\n\r\n";
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Request(r) => r,
            _ => panic!("expected request"),
        }
    }

    /// Build a b-leg with the three identity rewrites set as given.
    fn build(
        new_ruri: Option<&str>,
        new_from: Option<&str>,
        new_to: Option<&str>,
    ) -> Result<(Leg, OutboundSipEffect), UnreadableAddress> {
        build_b_leg(
            "w0|call-ref|xyz",
            "b-1",
            false,
            &a_leg_invite(),
            ("10.244.2.7".to_string(), 5060),
            new_ruri,
            new_from,
            new_to,
            None,
            &B2buaConfig::default(),
            &IdGen::seeded(0x055),
            None,
            &[],
            &CapabilitySet::default(),
            None, // no charging vector
            &[],
            &[], // no withheld option tags
            None,
        )
    }

    /// Addresses RFC 3261 §19.1.1 refuses, each of which `Uri::opaque` would
    /// have turned into a "host" equal to the whole string.
    const UNREADABLE: &[&str] = &[
        "sip:2001:db8::1",  // unbracketed IPv6 — would resolve "2001"
        "sip:host:88161",   // port out of range
        "not a uri at all", // no scheme
        "sip:[2001:db8::1", // unclosed IPv6 reference
    ];

    // Every identity rewrite is refused, and the error names WHICH field —
    // without that the operator sees a resolution failure with no defect in it.
    #[test]
    fn an_unreadable_identity_rewrite_refuses_the_leg_and_names_the_field() {
        for text in UNREADABLE {
            for (field, built) in [
                ("new_ruri", build(Some(text), None, None)),
                ("new_from", build(None, Some(text), None)),
                ("new_to", build(None, None, Some(text))),
            ] {
                let err = built
                    .err()
                    .unwrap_or_else(|| panic!("{field}={text:?} must be refused, not routed"));
                assert_eq!(err.field, field);
                assert_eq!(err.value, *text, "the refusal carries the stated text");
                // The wire form names the field only — a peer's bytes never
                // reach a reason phrase through it.
                assert_eq!(err.to_string(), format!("Unreadable Routing Address ({field})"));
                assert!(err.detail().contains(text), "the log detail keeps the value");
            }
        }
    }

    // The refusal is exact: a readable rewrite still builds, and the b-leg
    // carries it. A blanket refusal would break every identity rewrite.
    #[test]
    fn a_readable_identity_rewrite_still_builds_the_leg() {
        let (leg, effect) = build(
            Some("sip:charlie@10.244.2.9:5060"),
            Some("sip:+15551234@carrier.example"),
            Some("sip:+15559876@carrier.example"),
        )
        .expect("a readable rewrite must route");
        assert_eq!(leg.invite_request_uri.as_deref(), Some("sip:charlie@10.244.2.9:5060"));
        let invite = match effect.body {
            OutboundBody::Request(r) => r,
            OutboundBody::Response(_) | OutboundBody::Datagram(_) => {
                panic!("b-leg effect must carry a request")
            }
        };
        assert_eq!(invite.from().uri().host(), "carrier.example");
        assert_eq!(invite.to().uri().user(), Some("+15559876"));
    }

    fn invite_of(effect: OutboundSipEffect) -> SipRequest {
        match effect.body {
            OutboundBody::Request(r) => r,
            OutboundBody::Response(_) | OutboundBody::Datagram(_) => {
                panic!("b-leg effect must carry a request")
            }
        }
    }

    /// A `tag` a decision states on its From is dropped: the leg's tag is the
    /// core's own, exactly one on the wire, and the URI parameters inside the
    /// brackets stay URI parameters.
    #[test]
    fn a_decision_from_tag_is_dropped_for_the_minted_one() {
        let (leg, effect) =
            build(None, Some("<sip:+15551234@carrier.example;user=phone>;tag=stated-tag"), None)
                .expect("a bracketed From must route");
        let invite = invite_of(effect);
        let tag = invite.from().tag().expect("a From tag").to_string();
        assert_ne!(tag, "stated-tag");
        assert_eq!(leg.dialogs[0].sip.local_tag, tag);
        assert_eq!(invite.from().uri().user(), Some("+15551234"));
        assert_eq!(invite.from().uri().param("user").and_then(|p| p.as_str()), Some("phone"));
        let from_line = invite
            .headers()
            .iter()
            .find(|h| h.name.as_str().eq_ignore_ascii_case("From"))
            .map(|h| h.value.as_str().to_string())
            .expect("From line");
        assert_eq!(from_line.matches("tag=").count(), 1, "{from_line}");
    }

    /// A request outside any dialog carries no To tag (RFC 3261 §8.1.1.2): one a
    /// decision states is dropped, the rest of the To kept.
    #[test]
    fn a_decision_to_tag_is_dropped() {
        let (_leg, effect) =
            build(None, None, Some("<sip:+15559876@carrier.example;user=phone>;tag=peer-tag"))
                .expect("a bracketed To must route");
        let invite = invite_of(effect);
        assert_eq!(invite.to().tag(), None);
        assert_eq!(invite.to().uri().user(), Some("+15559876"));
        assert_eq!(invite.to().uri().param("user").and_then(|p| p.as_str()), Some("phone"));
    }

    /// A `tag` written as a URI parameter, on a bare value or inside the brackets,
    /// is dropped the same way; the other URI parameters stay.
    #[test]
    fn a_tag_uri_parameter_is_dropped() {
        for text in [
            "sip:+15551234@carrier.example;user=phone;tag=bare-tag",
            "<sip:+15551234@carrier.example;user=phone;tag=inner>",
        ] {
            let (_leg, effect) = build(None, Some(text), Some(text)).expect("must route");
            let invite = invite_of(effect);
            assert!(!invite.from().uri().params().has("tag"), "{text}");
            assert!(!invite.to().uri().params().has("tag"), "{text}");
            assert_eq!(invite.to().tag(), None);
            assert!(invite.from().tag().is_some_and(|t| !t.is_empty() && t != "bare-tag"));
            assert_eq!(invite.to().uri().param("user").and_then(|p| p.as_str()), Some("phone"));
        }
    }

    /// A bracketed identity no reader accepts is refused like a bare one, the field
    /// named.
    #[test]
    fn an_unreadable_bracketed_identity_is_refused() {
        for text in ["<sip:a@2001:db8::1>", "\"Bob\" <sip:a@h", "<>"] {
            for (field, built) in [
                ("new_from", build(None, Some(text), None)),
                ("new_to", build(None, None, Some(text))),
            ] {
                let err = built.err().unwrap_or_else(|| panic!("{field}={text:?} must be refused"));
                assert_eq!(err.field, field);
                assert_eq!(err.value, text);
            }
        }
    }

    /// A `tag=` inside the userinfo or a `?`-header value would count as a second
    /// From tag at the freeze, so the identity is refused on either field, never built.
    #[test]
    fn a_tag_inside_the_userinfo_is_refused() {
        for text in [
            "<sip:+15551234;tag=inner@carrier.example>",
            "<sip:+15551234@carrier.example?x=a;tag=b>",
        ] {
            for (field, built) in [
                ("new_from", build(None, Some(text), None)),
                ("new_to", build(None, None, Some(text))),
            ] {
                let err = built.err().unwrap_or_else(|| panic!("{field}={text:?} must be refused"));
                assert_eq!(err.field, field);
            }
        }
    }

    // No rewrites at all: the relayed a-leg values were admitted by the inbound
    // parser, so nothing is re-read and nothing can be refused.
    #[test]
    fn relayed_a_leg_addresses_are_never_refused() {
        let (_leg, effect) = build(None, None, None).expect("relayed a-leg values must route");
        let invite = match effect.body {
            OutboundBody::Request(r) => r,
            OutboundBody::Response(_) | OutboundBody::Datagram(_) => {
                panic!("b-leg effect must carry a request")
            }
        };
        assert_eq!(invite.request_uri().host_port(), ("10.244.2.7", 5060));
    }

    // A 3xx Contact is what the caller dials next, so an unreadable redirect
    // target is refused rather than emitted as an opaque URI.
    #[test]
    fn an_unreadable_redirect_target_is_refused() {
        for text in UNREADABLE {
            let err = redirect_contact(text, Some(0.5))
                .err()
                .unwrap_or_else(|| panic!("redirect target {text:?} must be refused"));
            assert_eq!(err.field, "contact");
        }
        let header = redirect_contact("sip:carol@10.244.2.11:5060", Some(0.7))
            .expect("a readable redirect target must render");
        assert!(header.value.contains("sip:carol@10.244.2.11:5060"));
        assert!(header.value.contains("q=0.7"));
    }
}
