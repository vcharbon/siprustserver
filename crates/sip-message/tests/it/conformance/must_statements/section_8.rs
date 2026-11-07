//! RFC 3261 §8: what a request and a response must hold.

use super::*;

/// RFC 3261 §8: what a request and a response must hold.
pub(super) fn rows() -> Vec<Row> {
    vec![
        // §8.1.1: the mandatory fields.
        dropped_because("3261-8.1.1-no-to", "To", options().with("to", "X-To: a")),
        dropped_because("3261-8.1.1-no-from", "From", options().with("from", "X-From: a")),
        dropped_because(
            "3261-8.1.1-no-call-id",
            "Call-ID",
            options().with("call-id", "X-Call-ID: a"),
        ),
        dropped_because("3261-8.1.1-no-cseq", "CSeq", options().with("cseq", "X-CSeq: a")),
        dropped_because("3261-8.1.1-no-via", "Via", options().with("via", "X-Via: a")),
        // §8.1.1, ADR-0001: no RFC 2543 compatibility, so no fallback either.
        answered_because(
            "3261-8.1.1-no-max-forwards",
            "Max-Forwards",
            options().with("max-forwards", "X-Max-Forwards: 70"),
        ),
        // §8.1.1.3: a From carries a tag.
        refused_any("3261-8.1.1.3-from-no-tag", options().with("from", NO_TAG_FROM)),
        // §8.1.1.5: the number's range, and the method the request line names.
        accept("3261-8.1.1.5-cseq-zero", options().with("cseq", "CSeq: 0 OPTIONS")),
        // §20.16, RFC 4475 §3.1.2.4: any 32-bit unsigned value reads, in a dialog
        // or out of one; §8.1.1.5's bound on a new request's first number is the
        // sender's.
        accept("3261-20.16-cseq-at-2-31", options().with("cseq", "CSeq: 2147483648 OPTIONS")),
        accept("3261-20.16-cseq-highest", options().with("cseq", "CSeq: 4294967295 OPTIONS")),
        dropped(
            "3261-20.16-cseq-past-the-range",
            options().with("cseq", "CSeq: 4294967296 OPTIONS"),
        ),
        // ADR-0054 decides the outcome over RFC 4475 §3.1.2.17's SHOULD of a 400:
        // a CSeq naming another method echoes nothing a transaction matches.
        dropped("3261-8.1.1.5-cseq-other-method", options().with("cseq", "CSeq: 1 BYE")),
        // The same range in a dialog.
        accept("3261-20.16-cseq-in-dialog-at-2-31", in_dialog("BYE", "CSeq: 2147483648 BYE")),
        accept("3261-20.16-cseq-in-dialog-highest", in_dialog("BYE", "CSeq: 4294967295 BYE")),
        dropped(
            "3261-20.16-cseq-in-dialog-past-the-range",
            in_dialog("BYE", "CSeq: 4294967296 BYE"),
        ),
        // §20.19: Expires is a 32-bit unsigned integer.
        accept("3261-20.19-expires-highest", options().add("Expires: 4294967295")),
        answered("3261-20.19-expires-past-the-range", options().add("Expires: 4294967296")),
        // §20.22: Max-Forwards' range.
        accept("3261-20.22-max-forwards-zero", options().with("max-forwards", "Max-Forwards: 0")),
        accept(
            "3261-20.22-max-forwards-highest",
            options().with("max-forwards", "Max-Forwards: 255"),
        ),
        answered(
            "3261-20.22-max-forwards-past-the-range",
            options().with("max-forwards", "Max-Forwards: 256"),
        ),
        // ADR-0001: the magic cookie, the transports, ports and octets.
        dropped(
            "adr-0001-top-via-no-branch",
            options().with("via", "Via: SIP/2.0/UDP 192.0.2.10:5060"),
        ),
        dropped(
            "adr-0001-top-via-no-cookie",
            options().with("via", "Via: SIP/2.0/UDP 192.0.2.10:5060;branch=old-style"),
        ),
        dropped(
            "adr-0001-top-via-bare-cookie",
            options().with("via", "Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK"),
        ),
        dropped(
            "adr-0001-top-via-unknown-transport",
            options().with("via", "Via: SIP/2.0/XYZ 192.0.2.10:5060;branch=z9hG4bK-a"),
        ),
        accept(
            "adr-0001-lower-via-any-transport",
            options().with("lower-via", "Via: SIP/2.0/XYZ 192.0.2.11:5060;branch=old-style"),
        ),
        dropped(
            "adr-0001-top-via-port-zero",
            options().with("via", "Via: SIP/2.0/UDP 192.0.2.10:0;branch=z9hG4bK-a"),
        ),
        accept(
            "adr-0001-lower-via-port-zero",
            options().with("lower-via", "Via: SIP/2.0/UDP 192.0.2.11:0;branch=z9hG4bK-b"),
        ),
        dropped(
            "adr-0001-via-port-past-the-range",
            options().with("via", "Via: SIP/2.0/UDP 192.0.2.10:65536;branch=z9hG4bK-a"),
        ),
        dropped(
            "adr-0001-octet-past-the-range",
            options().with("via", "Via: SIP/2.0/UDP 192.0.2.256:5060;branch=z9hG4bK-a"),
        ),
        dropped(
            "adr-0001-octet-leading-zero",
            options().with("via", "Via: SIP/2.0/UDP 192.0.2.010:5060;branch=z9hG4bK-a"),
        ),
        // §25.1, Via: what the one reader of it holds.
        accept(
            "3261-25.1-via-received-unbracketed-v6",
            options().with(
                "lower-via",
                "Via: SIP/2.0/UDP 192.0.2.11:5060;received=2001:db8::9;branch=z9hG4bK-b",
            ),
        ),
        accept(
            "3261-25.1-via-maddr-v6",
            options().with(
                "lower-via",
                "Via: SIP/2.0/UDP 192.0.2.11:5060;maddr=[2001:db8::9];branch=z9hG4bK-b",
            ),
        ),
        dropped(
            "3261-25.1-lower-via-transport-with-slash",
            options().with("lower-via", "Via: SIP/2.0/UDP/foo 192.0.2.11;branch=z9hG4bK-b"),
        ),
        dropped(
            "3261-25.1-lower-via-transport-non-token",
            options().with("lower-via", "Via: SIP/2.0/X(y) 192.0.2.11;branch=z9hG4bK-b"),
        ),
        dropped(
            "3261-25.1-via-junk-between-parameters",
            options().with("via", "Via: SIP/2.0/UDP 192.0.2.10;branch=z9hG4bK-a garbage;rport"),
        ),
        // §8.1.1.8, §12.1: one Contact on what can establish a dialog.
        answered_because("3261-8.1.1.8-invite-no-contact", "Contact", without_contact("INVITE")),
        answered_because("6665-8.1-subscribe-no-contact", "Contact", without_contact("SUBSCRIBE")),
        answered_because("6665-8.1-notify-no-contact", "Contact", without_contact("NOTIFY")),
        answered_because("3515-2.1-refer-no-contact", "Contact", without_contact("REFER")),
        dropped_because(
            "3261-12.1.1-2xx-invite-no-contact",
            "Contact",
            response_without_contact("INVITE"),
        ),
        dropped_because(
            "6665-8.1-2xx-subscribe-no-contact",
            "Contact",
            response_without_contact("SUBSCRIBE"),
        ),
        dropped_because(
            "3515-2.1-2xx-refer-no-contact",
            "Contact",
            response_without_contact("REFER"),
        ),
        accept("6665-8.1-2xx-notify-no-contact", response(200, "NOTIFY")),
        accept("3261-13.2.1-1xx-no-contact", response(180, "INVITE")),
        answered(
            "3261-8.1.1.8-invite-two-contacts",
            request("INVITE").add("Contact: <sip:carol@192.0.2.30:5060>"),
        ),
        accept(
            "3261-20.10-register-two-contacts",
            request("REGISTER")
                .add("Contact: <sip:a@192.0.2.30>")
                .add("Contact: <sip:b@192.0.2.30>"),
        ),
        answered(
            "rule-contact-no-user-host",
            request("INVITE").with("contact", "Contact: <sip:@192.0.2.30>"),
        ),
        answered(
            "rule-contact-unterminated-quote",
            request("INVITE").with("contact", "Contact: \"a <sip:carol@192.0.2.30>"),
        ),
        holds(
            "3261-20.10-contact-star",
            request("REGISTER").with("contact", "Contact: *").add("Expires: 0"),
            typed::<header::Contact>,
        ),
        accept(
            "adr-0053-5-3xx-two-contacts",
            response(302, "INVITE")
                .add("Contact: <sip:a@192.0.2.30>")
                .add("Contact: <sip:b@192.0.2.30>"),
        ),
        // §8.2.6.2: a To-tag on a response past 100.
        dropped(
            "3261-8.2.6.2-final-no-to-tag",
            response(486, "INVITE").with("to", "To: <sip:bob@example.com>"),
        ),
        dropped(
            "3261-12.1.1-2xx-invite-no-to-tag",
            response(200, "INVITE").with("to", "To: <sip:bob@example.com>"),
        ),
        // No exemption: every response but a 100 carries a To-tag, whatever it ends.
        dropped(
            "3261-8.2.6.2-2xx-options-no-to-tag",
            response(200, "OPTIONS").with("to", "To: <sip:bob@example.com>"),
        ),
        dropped(
            "3261-8.2.6.2-2xx-cancel-no-to-tag",
            response(200, "CANCEL").with("to", "To: <sip:bob@example.com>"),
        ),
        accept(
            "3261-8.2.6.1-100-no-to-tag",
            response(100, "INVITE").with("to", "To: <sip:bob@example.com>"),
        ),
        // §19.3: a tag is a token, one per header.
        dropped(
            "3261-19.3-from-empty-tag",
            options().with("from", "From: <sip:alice@example.com>;tag="),
        ),
        dropped(
            "3261-19.3-from-two-tags",
            options().with("from", "From: <sip:alice@example.com>;tag=a;tag=b"),
        ),
        dropped(
            "3261-19.3-to-two-tags",
            options().with("to", "To: <sip:bob@example.com>;tag=a;tag=b"),
        ),
        // §20.20, §25.1: the bare forms the ABNF admits.
        accept(
            "3261-20.20-bare-from-quoted-at-in-parameter",
            options().with("from", "From: sip:example.com;x=\"a@b\";tag=1"),
        ),
    ]
}
