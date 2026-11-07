//! The RFC's MUST and MUST NOT statements on message syntax, section by section,
//! and the shapes the RFC and the stated rules name. A row states what the text
//! says: this form reads, or this form is refused, answered or dropped as
//! ADR-0054 gives. ADR-0001's refusals are rows too, and so are the shapes the
//! stack's own rules name (`rule-*`).

use sip_message::header;
use sip_message::sipfrag;

use super::break_rule::refused;
use super::message::{read, reads_every_header, typed, ReadCheck, Template};
use super::ratchet::{settle, Row};

fn options() -> Template {
    Template::request("OPTIONS")
}

fn request(method: &str) -> Template {
    Template::request(method)
}

fn response(status: u16, method: &str) -> Template {
    Template::response(status, method)
}

/// The message reads and every typed reader reads it.
fn reads(raw: &[u8]) -> Result<(), String> {
    match read(raw) {
        Ok(msg) => reads_every_header(&msg),
        Err(e) => Err(e.reason),
    }
}

fn accept(name: &str, template: Template) -> Row {
    Row::new(format!("must/{name}"), reads(&template.bytes()))
}

fn accept_raw(name: &str, raw: &[u8]) -> Row {
    Row::new(format!("must/{name}"), reads(raw))
}

/// A line that breaks a rule, whose message is answered 400.
fn answered(name: &str, template: Template) -> Row {
    Row::new(format!("must/{name}"), refused(&template.bytes(), true))
}

/// A line that leaves no partial: echoed, or the message is a response.
fn dropped(name: &str, template: Template) -> Row {
    Row::new(format!("must/{name}"), refused(&template.bytes(), false))
}

/// The line is refused, or the typed reader of a header ingress does not vouch
/// for refuses it.
fn unread(name: &str, template: Template, read_it: ReadCheck) -> Row {
    let outcome = match read(&template.bytes()) {
        Ok(msg) => read_it(&msg).err().map_or(Err("read".to_string()), |_| Ok(())),
        Err(_) => Ok(()),
    };
    Row::new(format!("must/{name}"), outcome)
}

/// `check` holds of the message `template` builds.
fn holds(name: &str, template: Template, check: ReadCheck) -> Row {
    let outcome = match read(&template.bytes()) {
        Ok(msg) => check(&msg),
        Err(e) => Err(e.reason),
    };
    Row::new(format!("must/{name}"), outcome)
}

const NO_TAG_FROM: &str = "From: <sip:alice@example.com>";

/// RFC 3261 §7: the message's shape and its header fields.
fn section_7() -> Vec<Row> {
    let mut rows = vec![
        // §7.1: SIP-Version is compared case-insensitively.
        accept("3261-7.1-version-lower-request", options().start_line("OPTIONS sip:bob@example.com sip/2.0")),
        accept("3261-7.1-version-mixed-request", options().start_line("OPTIONS sip:bob@example.com Sip/2.0")),
        accept("3261-7.1-version-lower-response", response(200, "OPTIONS").start_line("sip/2.0 200 OK")),
        Row::new("must/3261-7.1-sipfrag-version-lower", match sipfrag::status(b"sip/2.0 200 OK\r\n") {
            Some(200) => Ok(()),
            other => Err(format!("status {other:?}")),
        }),
        dropped("4475-3.1.2.16-version-unknown", options().start_line("OPTIONS sip:bob@example.com SIP/7.0")),
        // ADR-0001, status codes: §7.2 gives six values for the first digit.
        accept("3261-7.2-status-lowest", response(100, "OPTIONS").start_line("SIP/2.0 100 Trying")),
        accept("3261-7.2-status-highest", response(699, "OPTIONS").start_line("SIP/2.0 699 Reason")),
        dropped("adr-0001-status-below-100", response(200, "OPTIONS").start_line("SIP/2.0 099 Reason")),
        dropped("adr-0001-status-above-699", response(200, "OPTIONS").start_line("SIP/2.0 700 Reason")),
        // §7.3.1: the header field format.
        accept("3261-7.3.1-space-before-colon", options().add("Subject : a")),
        accept("3261-7.3.1-tab-before-colon", options().add("Subject\t: a")),
        accept("3261-7.3.1-no-space-after-colon", options().add("Subject:a")),
        accept("3261-7.3.1-fold-after-colon", options().add("Subject:\r\n a")),
        accept("3261-7.3.1-name-any-case", options().add("sUbJeCt: a")),
        dropped("3261-7.3.1-fold-before-colon", options().with("from", "From\r\n : <sip:alice@example.com>;tag=a1")),
        accept("3261-7.3.1-route-on-two-lines", options().add("Route: <sip:a.example.com;lr>").add("Route: <sip:b.example.com;lr>")),
        accept("3261-7.3.1-route-on-one-line", options().add("Route: <sip:a.example.com;lr>, <sip:b.example.com;lr>")),
        accept("3261-7.3.1-via-on-one-line", options().with("via", "Via: SIP/2.0/UDP 192.0.2.10;branch=z9hG4bK-a, SIP/2.0/UDP 192.0.2.11;branch=z9hG4bK-b")),
        accept("3261-20.14-trailing-space-content-length", options().unframed().add("Content-Length: 0 ")),
        accept("3261-25.1-trailing-space-via", options().with("via", "Via: SIP/2.0/UDP 192.0.2.10;branch=z9hG4bK-a  ")),
        accept("3261-25.1-trailing-space-from", options().with("from", "From: <sip:alice@example.com>;tag=a1\t")),
        accept("3261-25.1-trailing-space-call-id", options().with("call-id", "Call-ID: abc@example.com ")),
        accept("3261-25.1-trailing-space-cseq", options().with("cseq", "CSeq: 1 OPTIONS ")),
        accept("3261-25.1-trailing-fold", options().with("from", "From: <sip:alice@example.com>;tag=a1\r\n ")),
        // §7.3.3: every compact form of §20 and the extension RFCs.
    ];
    let compact: [(&str, &str, ReadCheck); 4] = [
        ("o", "refer", typed::<header::Event>),
        ("u", "refer", typed::<header::AllowEvents>),
        ("r", "<sip:carol@example.com>", typed::<header::ReferTo>),
        ("b", "<sip:carol@example.com>", typed::<header::ReferredBy>),
    ];
    for (letter, value, read) in compact {
        rows.push(holds(
            &format!("3261-7.3.3-compact-{letter}"),
            options().add(format!("{letter}: {value}")),
            read,
        ));
    }
    rows.extend([
        // §7.3.1: a value the grammar lets be empty, and one it does not.
        accept("3261-25.1-empty-supported", options().add("Supported:")),
        accept("3261-25.1-empty-allow", options().add("Allow:")),
        accept("3261-25.1-empty-accept-encoding", options().add("Accept-Encoding:")),
        accept("3261-25.1-empty-accept", options().add("Accept:")),
        accept("3261-25.1-empty-subject", options().add("Subject:")),
        accept("5009-9-empty-p-early-media", options().add("P-Early-Media:")),
        unread("3261-25.1-empty-require", options().add("Require:"), typed::<header::Require>),
        unread(
            "3261-25.1-empty-proxy-require",
            options().add("Proxy-Require:"),
            typed::<header::ProxyRequire>,
        ),
        unread(
            "3261-25.1-empty-unsupported",
            options().add("Unsupported:"),
            typed::<header::Unsupported>,
        ),
        unread(
            "6665-8.4-empty-allow-events",
            options().add("Allow-Events:"),
            typed::<header::AllowEvents>,
        ),
        dropped("3261-25.1-empty-via-beside-a-via", options().add("Via:")),
        answered("3261-25.1-empty-contact", request("REGISTER").with("contact", "Contact:")),
        answered("3261-25.1-empty-route", options().add("Route:")),
        answered("3261-25.1-empty-record-route", options().add("Record-Route:")),
        unread(
            "3325-9.1-empty-p-asserted-identity",
            options().add("P-Asserted-Identity:"),
            typed::<header::PAssertedIdentity>,
        ),
        answered("3261-25.1-empty-content-type", options().add("Content-Type:")),
        unread(
            "3261-25.1-empty-content-disposition",
            options().add("Content-Disposition:"),
            typed::<header::ContentDisposition>,
        ),
        // A descriptive header nothing reads passes repeated.
        accept("rule-repeat-subject", options().add("Subject: a").add("Subject: b")),
        accept("rule-repeat-user-agent", options().add("User-Agent: a").add("User-Agent: b")),
        accept("rule-repeat-server", options().add("Server: a").add("Server: b")),
        accept("rule-repeat-organization", options().add("Organization: a").add("Organization: b")),
        accept(
            "rule-repeat-date",
            options()
                .add("Date: Sat, 13 Nov 2010 23:29:00 GMT")
                .add("Date: Sat, 13 Nov 2010 23:29:01 GMT"),
        ),
        accept(
            "rule-repeat-p-asserted-identity",
            options()
                .add("P-Asserted-Identity: <sip:a@example.com>")
                .add("P-Asserted-Identity: <tel:+15551234>"),
        ),
        accept(
            "rule-repeat-call-info",
            options()
                .add("Call-Info: <http://example.com/a>")
                .add("Call-Info: <http://example.com/b>"),
        ),
        // §25.1: a word may hold a DQUOTE that opens nothing.
        accept(
            "3261-25.1-word-with-a-dquote-then-a-control-byte",
            request("INVITE").add(b"Replaces: a\"b;to-tag=1;from-tag=2;x=\"\\\x04\"".to_vec()),
        ),
        // §25.1: an NBSP is content of TEXT-UTF8-TRIM, at an edge or inside.
        accept("3261-25.1-nbsp-in-a-subject", options().add("Subject: a\u{a0}b")),
        accept("3261-25.1-nbsp-at-a-subject-edge", options().add("Subject: \u{a0}a\u{a0}")),
        // §25.1: TEXT-UTF8-TRIM has no quoted-string.
        accept(
            "3261-25.1-subject-unbalanced-quote",
            options().add("Subject: The caller said \"hello"),
        ),
    ]);
    rows
}

/// RFC 3261 §8: what a request and a response must hold.
fn section_8() -> Vec<Row> {
    vec![
        // §8.1.1: the mandatory fields.
        dropped("3261-8.1.1-no-to", options().with("to", "X-To: a")),
        dropped("3261-8.1.1-no-from", options().with("from", "X-From: a")),
        dropped("3261-8.1.1-no-call-id", options().with("call-id", "X-Call-ID: a")),
        dropped("3261-8.1.1-no-cseq", options().with("cseq", "X-CSeq: a")),
        dropped("3261-8.1.1-no-via", options().with("via", "X-Via: a")),
        answered(
            "3261-8.1.1-no-max-forwards",
            options().with("max-forwards", "X-Max-Forwards: 70"),
        ),
        // §8.1.1.3: a From carries a tag.
        dropped("3261-8.1.1.3-from-no-tag", options().with("from", NO_TAG_FROM)),
        // §8.1.1.5: the number's range, and the method the request line names.
        accept("3261-8.1.1.5-cseq-zero", options().with("cseq", "CSeq: 0 OPTIONS")),
        accept("3261-8.1.1.5-cseq-highest", options().with("cseq", "CSeq: 2147483647 OPTIONS")),
        dropped(
            "3261-8.1.1.5-cseq-past-the-range",
            options().with("cseq", "CSeq: 2147483648 OPTIONS"),
        ),
        dropped("3261-8.1.1.5-cseq-other-method", options().with("cseq", "CSeq: 1 BYE")),
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
        answered("3261-8.1.1.8-invite-no-contact", without_contact("INVITE")),
        answered("6665-8.1-subscribe-no-contact", without_contact("SUBSCRIBE")),
        answered("6665-8.1-notify-no-contact", without_contact("NOTIFY")),
        answered("3515-2.1-refer-no-contact", without_contact("REFER")),
        dropped("3261-12.1.1-2xx-invite-no-contact", response_without_contact("INVITE")),
        dropped("6665-8.1-2xx-subscribe-no-contact", response_without_contact("SUBSCRIBE")),
        dropped("3515-2.1-2xx-refer-no-contact", response_without_contact("REFER")),
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

fn without_contact(method: &str) -> Template {
    request(method).with("contact", "X-Contact: none")
}

fn response_without_contact(method: &str) -> Template {
    response(200, method).with("contact", "X-Contact: none")
}

/// RFC 3261 §18.3, §19.1.1, §20.14, §20.15 and §25.1: framing, URIs and bytes.
fn sections_18_to_25() -> Vec<Row> {
    let head = options();
    vec![
        // §18.3, §20.14: what frames the body.
        answered(
            "3261-18.3-content-length-past-the-datagram",
            head.clone().unframed().add("Content-Length: 5").body("abc"),
        ),
        accept(
            "3261-18.3-content-length-short-of-the-datagram",
            head.clone()
                .unframed()
                .add("Content-Type: text/plain")
                .add("Content-Length: 1")
                .body("abc"),
        ),
        answered(
            "4475-3.1.2.3-content-length-negative",
            head.clone().unframed().add("Content-Length: -1"),
        ),
        accept("3261-18.3-no-content-length", head.clone().unframed()),
        // §19.1.1 Table 1, ADR-0053 §2: a Request-URI's form.
        answered(
            "adr-0053-2-request-uri-headers",
            head.clone().start_line("OPTIONS sip:bob@example.com?Subject=a SIP/2.0"),
        ),
        answered(
            "adr-0053-2-request-uri-empty-host",
            head.clone().start_line("OPTIONS sip:bob@ SIP/2.0"),
        ),
        answered("rule-route-opaque-scheme", head.clone().add("Route: <urn:service:sos>")),
        answered("adr-0053-2-route-tel", head.clone().add("Route: <tel:+15551234;lr>")),
        // §25.1: bytes of a value.
        dropped("3261-25.1-bare-cr", head.clone().add("Subject: a\rb")),
        dropped("3261-25.1-bare-lf", head.clone().add("Subject: a\nb")),
        answered("3261-25.1-nul-in-value", head.clone().add("Subject: a\0b")),
        answered("3261-25.1-del-in-value", head.clone().add("Subject: a\x7fb")),
        dropped("adr-0054-2-non-utf8-in-a-name", head.clone().add(b"Vi\xffa: x".to_vec())),
        accept(
            "4475-3.1.1.2-quoted-pair-control",
            head.clone().add(b"X-Note: \"\\\x07\"".to_vec()),
        ),
        answered(
            "3261-25.1-quoted-pair-where-no-quoted-string",
            head.clone().add(b"Organization: \"a\\\0b\"".to_vec()),
        ),
        answered(
            "3261-25.1-quoted-pair-of-non-ascii",
            head.clone().add("Reply-To: \"a\\\u{e9}\" <sip:a@example.com>"),
        ),
        answered(
            "3261-25.1-quoted-pair-of-a-fold",
            head.clone().add("Reply-To: \"a\\\r\n b\" <sip:a@example.com>"),
        ),
        unread(
            "3261-25.1-unterminated-quote-disposition",
            head.clone().add("Content-Disposition: session;handling=\"x"),
            typed::<header::ContentDisposition>,
        ),
        dropped(
            "3261-25.1-unterminated-quote-via",
            head.clone().with("lower-via", "Via: SIP/2.0/UDP 192.0.2.11;branch=z9hG4bK-b;x=\"abc"),
        ),
        answered(
            "3261-25.1-unterminated-quote-content-type",
            head.clone().add("Content-Type: multipart/mixed;boundary=\"abc"),
        ),
        unread(
            "3261-25.1-unterminated-quote-event",
            head.clone().add("Event: a;x=\"b, c"),
            typed::<header::Event>,
        ),
        answered(
            "4028-4-unterminated-quote-session-expires",
            head.clone().add("Session-Expires: 90;x=\"abc"),
        ),
        // ADR-0053 §3: what the header block's bytes may be. A well-formed surrogate
        // pair is ordinary non-ASCII where the grammar allows it.
        accept(
            "3261-25.1-utf8-display-name",
            head.clone().with("from", "From: \"caf\u{e9}\" <sip:alice@example.com>;tag=a1"),
        ),
        accept_raw(
            "rule-cesu8-pair-in-a-display-name",
            &head
                .clone()
                .with(
                    "from",
                    b"From: \"\xed\xa0\xbc\xed\xbc\xb9\" <sip:alice@example.com>;tag=a1".to_vec(),
                )
                .bytes(),
        ),
        answered("adr-0053-3-latin1-byte", head.clone().add(b"Subject: caf\xe9".to_vec())),
        answered(
            "adr-0053-3-lone-surrogate-half",
            head.clone().add(b"Subject: \xed\xa0\xbc".to_vec()),
        ),
        answered("adr-0053-3-overlong-form", head.clone().add(b"Subject: \xc0\xaf".to_vec())),
        answered(
            "adr-0053-3-five-byte-form",
            head.clone().add(b"Subject: \xf8\x88\x80\x80\x80".to_vec()),
        ),
        dropped(
            "rule-cesu8-pair-in-a-tag",
            head.clone().with(
                "from",
                b"From: <sip:alice@example.com>;tag=\xed\xa0\xbc\xed\xbc\xb9".to_vec(),
            ),
        ),
        dropped(
            "rule-cesu8-pair-in-a-call-id",
            head.clone().with("call-id", b"Call-ID: \xed\xa0\xbc\xed\xbc\xb9".to_vec()),
        ),
    ]
}

/// ADR-0001's first-ingress bound: a party's new request at its first hop.
fn bounds() -> Vec<Row> {
    let line = |length: usize| format!("Subject: {}", "x".repeat(length - "Subject: ".len()));
    let with_lines = |count: usize| (0..count).fold(options(), |t, _| t.add("Subject: a"));
    // The template's own lines, the start line and its Content-Length among them.
    let own = options().bytes().iter().filter(|b| **b == b'\n').count() - 1;
    let uri = |length: usize| {
        let host = "x".repeat(length - "sip:bob@".len());
        format!("OPTIONS sip:bob@{host} SIP/2.0")
    };
    vec![
        accept("adr-0001-line-at-the-bound", options().add(line(8192))),
        answered("adr-0001-line-past-the-bound", options().add(line(8193))),
        accept("adr-0001-request-uri-at-the-bound", options().start_line(uri(8192))),
        answered("adr-0001-request-uri-past-the-bound", options().start_line(uri(8193))),
        accept("adr-0001-block-at-the-bound", with_lines(100 - own)),
        answered("adr-0001-block-past-the-bound", with_lines(101 - own)),
    ]
}

/// The valid forms and refusals extension RFCs state.
fn extensions() -> Vec<Row> {
    let head = options();
    vec![
        // RFC 3261 §22.4, ADR-0001: credentials a reader takes whole.
        answered(
            "3261-22.4-credentials-no-comma",
            head.clone().add("Authorization: Digest realm=\"a\" nonce=\"b\""),
        ),
        answered(
            "3261-22.4-credentials-unquoted-space",
            head.clone().add("Authorization: Digest realm=a b"),
        ),
        answered(
            "3261-22.4-credentials-comment",
            head.clone().add("Authorization: Digest realm=(x)"),
        ),
        answered(
            "3261-22.4-credentials-empty-value",
            head.clone().add("Authorization: Digest realm="),
        ),
        // RFC 3261 §25.1: an option tag is a token.
        unread(
            "3261-25.1-option-tag-space",
            head.clone().add("Proxy-Require: foo bar"),
            typed::<header::ProxyRequire>,
        ),
        unread(
            "3261-25.1-option-tag-quoted",
            head.clone().add("Proxy-Require: \"foo\""),
            typed::<header::ProxyRequire>,
        ),
        unread(
            "3261-25.1-option-tag-slash",
            head.clone().add("Require: 100rel, foo/bar"),
            typed::<header::Require>,
        ),
        unread(
            "3261-25.1-option-tag-empty-entry",
            head.clone().add("Require: 100rel,,timer"),
            typed::<header::Require>,
        ),
        // RFC 3262 §7: the response number.
        accept("3262-7.1-rseq-lowest", response(183, "INVITE").add("RSeq: 1")),
        accept("3262-7.1-rseq-highest", response(183, "INVITE").add("RSeq: 2147483647")),
        dropped("3262-7.1-rseq-zero", response(183, "INVITE").add("RSeq: 0")),
        dropped("3262-7.1-rseq-not-digits", response(183, "INVITE").add("RSeq: abc")),
        dropped("3262-7.1-rseq-past-the-range", response(183, "INVITE").add("RSeq: 2147483648")),
        // RFC 3323 §4.2: `;` separates; a comma does not.
        accept("3323-4.2-privacy-semicolons", head.clone().add("Privacy: id;user")),
        answered("3323-4.2-privacy-comma", head.clone().add("Privacy: id, user")),
        // RFC 3325 §9.1: no header parameters, and a bare URI keeps its own.
        unread(
            "3325-9.1-pai-header-parameter",
            head.clone().add("P-Asserted-Identity: <sip:a@example.com>;foo=bar"),
            typed::<header::PAssertedIdentity>,
        ),
        holds(
            "3325-9.1-bare-tel-keeps-its-parameter",
            head.clone().add("P-Asserted-Identity: tel:+15551234;phone-context=x.example"),
            |msg| {
                let values = msg.list::<header::PAssertedIdentity>().map_err(|e| e.reason)?;
                let uri = values.first().ok_or("no value")?.uri().text().into_owned();
                uri.contains("phone-context").then_some(()).ok_or(format!("the URI is {uri}"))
            },
        ),
        // RFC 3326: not judged at ingress, and a cause reads.
        holds(
            "rule-reason-off-grammar-parameter",
            head.clone().add("Reason: Q.850;cause=16;host=<sip:a.example.com>"),
            |msg| {
                let reason =
                    msg.header::<header::Reason>().ok_or("no value")?.map_err(|e| e.reason)?;
                reason
                    .params()
                    .value("cause")
                    .filter(|c| *c == "16")
                    .map(|_| ())
                    .ok_or("no cause".to_string())
            },
        ),
        accept(
            "rule-charging-vector-unquoted-at",
            head.clone().add("P-Charging-Vector: icid-value=abc@example.com"),
        ),
        // RFC 3891 §6.1: exactly one to-tag and one from-tag, after one callid.
        unread(
            "3891-6.1-replaces-no-tags",
            request("INVITE").add("Replaces: abc@example.com"),
            typed::<header::Replaces>,
        ),
        unread(
            "3891-6.1-replaces-call-id-with-space",
            request("INVITE").add("Replaces: a b;to-tag=1;from-tag=2"),
            typed::<header::Replaces>,
        ),
        // RFC 4028: the session-timer tail.
        holds(
            "4028-5-min-se-parameter",
            request("INVITE").add("Min-SE: 90;foo=bar"),
            typed::<header::MinSe>,
        ),
        answered("4028-5-min-se-junk-parameters", request("INVITE").add("Min-SE: 90;=;;")),
        answered(
            "4028-4-session-expires-junk-parameters",
            request("INVITE").add("Session-Expires: 90;=;;"),
        ),
        // RFC 3261 §25.1: Retry-After's comment.
        holds(
            "3261-20.33-retry-after-comment-then-parameter",
            response(503, "OPTIONS").add("Retry-After: 120 (back; soon);duration=60"),
            |msg| {
                let retry =
                    msg.header::<header::RetryAfter>().ok_or("no value")?.map_err(|e| e.reason)?;
                retry
                    .params()
                    .value("duration")
                    .filter(|d| *d == "60")
                    .map(|_| ())
                    .ok_or(format!("read {}", sip_message::header::HeaderValue::to_wire(&retry)))
            },
        ),
        unread(
            "3261-20.33-retry-after-not-digits",
            response(503, "OPTIONS").add("Retry-After: abc"),
            typed::<header::RetryAfter>,
        ),
        // RFC 4488 §3, RFC 6665 §8.4: one leading token.
        unread(
            "4488-3-refer-sub-other-value",
            request("REFER").add("Refer-Sub: maybe x;y"),
            typed::<header::ReferSub>,
        ),
        unread(
            "6665-8.4-event-two-types",
            request("SUBSCRIBE").add("Event: a, b"),
            typed::<header::Event>,
        ),
        unread(
            "6665-8.4-event-space-in-the-type",
            request("SUBSCRIBE").add("Event: foo bar;id=1"),
            typed::<header::Event>,
        ),
        unread(
            "6665-8.4-allow-events-space-in-a-type",
            head.clone().add("Allow-Events: a b"),
            typed::<header::AllowEvents>,
        ),
    ]
}

#[test]
fn every_must_statement_holds() {
    let mut rows = section_7();
    rows.extend(section_8());
    rows.extend(sections_18_to_25());
    rows.extend(bounds());
    rows.extend(extensions());
    settle("must", &rows);
}
