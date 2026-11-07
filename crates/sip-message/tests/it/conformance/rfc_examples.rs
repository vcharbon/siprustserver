//! Must accept, from the RFCs: every header example of RFC 3261 §20 and of each
//! extension RFC the crate names, each in a whole message through the production
//! path, then through every typed reader.

use sip_message::SipMessage;

use super::message::{read, reads_every_header, Template};
use super::ratchet::{settle, Row};

/// The template that holds an example: the request or response its RFC puts the
/// header in, where that matters.
#[derive(Clone, Copy)]
enum Base {
    Options,
    Register,
    Invite,
    Prack,
    Refer,
    Notify,
    Subscribe,
    Response(u16, &'static str),
}

impl Base {
    fn template(self) -> Template {
        match self {
            Base::Options => Template::request("OPTIONS"),
            Base::Register => Template::request("REGISTER"),
            Base::Invite => Template::request("INVITE"),
            Base::Prack => Template::request("PRACK"),
            Base::Refer => Template::request("REFER"),
            Base::Notify => Template::request("NOTIFY"),
            Base::Subscribe => Template::request("SUBSCRIBE"),
            Base::Response(status, method) => Template::response(status, method),
        }
    }
}

use Base::{Invite, Notify, Options, Prack, Refer, Register, Response, Subscribe};

/// `(row, template line it replaces or "" to add, base, line)`, the line as the
/// RFC prints it: a printed break is a fold, but inside a URI the RFC's own
/// line wrap, which is joined.
const EXAMPLES: &[(&str, &str, Base, &str)] = &[
    ("3261-20.1-accept", "", Options, "Accept: application/sdp;level=1, application/x-private, text/html"),
    ("3261-20.2-accept-encoding", "", Options, "Accept-Encoding: gzip"),
    ("3261-20.3-accept-language", "", Options, "Accept-Language: da, en-gb;q=0.8, en;q=0.7"),
    ("3261-20.4-alert-info", "", Options, "Alert-Info: <http://www.example.com/sounds/moo.wav>"),
    ("3261-20.5-allow", "", Options, "Allow: INVITE, ACK, OPTIONS, CANCEL, BYE"),
    ("3261-20.6-authentication-info", "", Response(200, "OPTIONS"), "Authentication-Info: nextnonce=\"47364c23432d2e131a5fb210812c\""),
    ("3261-20.7-authorization", "", Options, "Authorization: Digest username=\"Alice\", realm=\"atlanta.com\",\r\n nonce=\"84a4cc6f3082121f32b42a2187831a9e\",\r\n response=\"7587245234b3434cc3412213e5f113a5432\""),
    ("3261-20.8-call-id", "call-id", Options, "Call-ID: f81d4fae-7dec-11d0-a765-00a0c91e6bf6@biloxi.com"),
    ("3261-20.8-call-id-compact", "call-id", Options, "i:f81d4fae-7dec-11d0-a765-00a0c91e6bf6@192.0.2.4"),
    ("3261-20.9-call-info", "", Options, "Call-Info: <http://wwww.example.com/alice/photo.jpg> ;purpose=icon,\r\n <http://www.example.com/alice/> ;purpose=info"),
    ("3261-20.10-contact-display", "contact", Register, "Contact: \"Mr. Watson\" <sip:watson@worcester.bell-telephone.com>\r\n ;q=0.7; expires=3600,\r\n \"Mr. Watson\" <mailto:watson@bell-telephone.com> ;q=0.1"),
    ("3261-20.10-contact-compact", "contact", Register, "m: <sips:bob@192.0.2.4>;expires=60"),
    ("3261-20.11-content-disposition", "", Options, "Content-Disposition: session"),
    ("3261-20.12-content-encoding", "", Options, "Content-Encoding: gzip"),
    ("3261-20.12-content-encoding-compact", "", Options, "e: tar"),
    ("3261-20.13-content-language", "", Options, "Content-Language: fr"),
    ("3261-20.15-content-type", "", Options, "Content-Type: application/sdp"),
    ("3261-20.15-content-type-compact", "", Options, "c: text/html; charset=ISO-8859-4"),
    ("3261-20.16-cseq", "cseq", Invite, "CSeq: 4711 INVITE"),
    ("3261-20.17-date", "", Options, "Date: Sat, 13 Nov 2010 23:29:00 GMT"),
    ("3261-20.18-error-info", "", Response(404, "OPTIONS"), "Error-Info: <sip:not-in-service-recording@atlanta.com>"),
    ("3261-20.19-expires", "", Options, "Expires: 5"),
    ("3261-20.20-from-display", "from", Options, "From: \"A. G. Bell\" <sip:agb@bell-telephone.com> ;tag=a48s"),
    ("3261-20.20-from-bare", "from", Options, "From: sip:+12125551212@server.phone2net.com;tag=887s"),
    ("3261-20.20-from-compact", "from", Options, "f: Anonymous <sip:c8oqz84zk7z@privacy.org>;tag=hyh8"),
    ("3261-20.21-in-reply-to", "", Options, "In-Reply-To: 70710@saturn.bell-tel.com, 17320@saturn.bell-tel.com"),
    ("3261-20.22-max-forwards", "max-forwards", Options, "Max-Forwards: 6"),
    ("3261-20.23-min-expires", "", Response(423, "REGISTER"), "Min-Expires: 60"),
    ("3261-20.24-mime-version", "", Options, "MIME-Version: 1.0"),
    ("3261-20.25-organization", "", Options, "Organization: Boxes by Bob"),
    ("3261-20.26-priority-emergency", "", Options, "Priority: emergency"),
    ("3261-20.26-priority-non-urgent", "", Options, "Priority: non-urgent"),
    ("3261-20.27-proxy-authenticate", "", Response(407, "OPTIONS"), "Proxy-Authenticate: Digest realm=\"atlanta.com\",\r\n domain=\"sip:ss1.carrier.com\", qop=\"auth\",\r\n nonce=\"f84f1cec41e6cbe5aea9c8e88d359\",\r\n opaque=\"\", stale=FALSE, algorithm=MD5"),
    ("3261-20.28-proxy-authorization", "", Options, "Proxy-Authorization: Digest username=\"Alice\", realm=\"atlanta.com\",\r\n nonce=\"c60f3082ee1212b402a21831ae\",\r\n response=\"245f23415f11432b3434341c022\""),
    ("3261-20.29-proxy-require", "", Options, "Proxy-Require: foo"),
    ("3261-20.30-record-route", "", Options, "Record-Route: <sip:server10.biloxi.com;lr>,\r\n <sip:bigbox3.site3.atlanta.com;lr>"),
    ("3261-20.31-reply-to", "", Options, "Reply-To: Bob <sip:bob@biloxi.com>"),
    ("3261-20.32-require", "", Options, "Require: 100rel"),
    ("3261-20.33-retry-after-duration", "", Response(503, "OPTIONS"), "Retry-After: 18000;duration=3600"),
    ("3261-20.33-retry-after-comment", "", Response(503, "OPTIONS"), "Retry-After: 120 (I'm in a meeting)"),
    ("3261-20.34-route", "", Options, "Route: <sip:bigbox3.site3.atlanta.com;lr>,\r\n <sip:server10.biloxi.com;lr>"),
    ("3261-20.35-server", "", Response(200, "OPTIONS"), "Server: HomeServer v2"),
    ("3261-20.36-subject", "", Options, "Subject: Need more boxes"),
    ("3261-20.36-subject-compact", "", Options, "s: Tech Support"),
    ("3261-20.37-supported", "", Options, "Supported: 100rel"),
    ("3261-20.38-timestamp", "", Options, "Timestamp: 54"),
    ("3261-20.39-to-tag", "to", Options, "To: The Operator <sip:operator@cs.columbia.edu>;tag=287447"),
    ("3261-20.39-to-compact", "to", Options, "t: sip:+12125551212@server.phone2net.com"),
    ("3261-20.40-unsupported", "", Response(420, "OPTIONS"), "Unsupported: foo"),
    ("3261-20.41-user-agent", "", Options, "User-Agent: Softphone Beta1.5"),
    ("3261-20.42-via-lower", "lower-via", Options, "Via: SIP/2.0/UDP erlang.bell-telephone.com:5060;branch=z9hG4bK87asdks7"),
    ("3261-20.42-via-received", "lower-via", Options, "Via: SIP/2.0/UDP 192.0.2.1:5060 ;received=192.0.2.207\r\n ;branch=z9hG4bK77asjd"),
    ("3261-20.42-via-compact", "lower-via", Options, "v: SIP / 2.0 / UDP first.example.com: 4000;ttl=16\r\n ;maddr=224.2.0.1 ;branch=z9hG4bKa7c6a8dlze.1"),
    ("3261-20.43-warning", "", Response(606, "INVITE"), "Warning: 307 isi.edu \"Session parameter 'foo' not understood\""),
    ("3261-20.43-warning-e164", "", Response(606, "INVITE"), "Warning: 301 isi.edu \"Incompatible network address type 'E.164'\""),
    ("3261-20.44-www-authenticate", "", Response(401, "OPTIONS"), "WWW-Authenticate: Digest realm=\"atlanta.com\",\r\n domain=\"sip:boxesbybob.com\", qop=\"auth\",\r\n nonce=\"f84f1cec41e6cbe5aea9c8e88d359\",\r\n opaque=\"\", stale=FALSE, algorithm=MD5"),
    ("3262-7.1-rseq", "rseq", Response(183, "INVITE"), "RSeq: 988789"),
    ("3262-7.2-rack", "", Prack, "RAck: 776656 1 INVITE"),
    ("3323-4.2-privacy-one", "", Options, "Privacy: id"),
    ("3325-9.1-pai-name-addr", "", Options, "P-Asserted-Identity: \"Cullen Jennings\" <sip:fluffy@cisco.com>"),
    ("3325-9.1-pai-tel", "", Options, "P-Asserted-Identity: tel:+14085264000"),
    ("3325-9.2-ppi", "", Options, "P-Preferred-Identity: \"Cullen Jennings\" <sip:fluffy@cisco.com>"),
    ("3326-2-reason-sip", "", Options, "Reason: SIP ;cause=200 ;text=\"Call completed elsewhere\""),
    ("3326-2-reason-q850", "", Options, "Reason: Q.850 ;cause=16 ;text=\"Terminated\""),
    ("3515-2.1-refer-to", "refer-to", Refer, "Refer-To: sip:alice@atlanta.example.com"),
    ("3515-2.1-refer-to-headers", "refer-to", Refer, "Refer-To: <sip:bob@biloxi.example.net?Accept-Contact=sip:bobsdesk.biloxi.example.net&Call-ID=55432%40alicepc.atlanta.example.com>"),
    ("3515-2.1-refer-to-replaces", "refer-to", Refer, "Refer-To: <sip:dave@denver.example.org?Replaces=12345%40192.168.118.3%3Bto-tag%3D12345%3Bfrom-tag%3D5FFE-3994>"),
    ("3515-2.1-refer-to-method", "refer-to", Refer, "Refer-To: <sip:carol@cleveland.example.org;method=SUBSCRIBE>"),
    ("3515-2.1-refer-to-http", "refer-to", Refer, "Refer-To: http://www.ietf.org"),
    ("3581-4-rport-flag", "via", Options, "Via: SIP/2.0/UDP 10.1.1.1:4540;rport;branch=z9hG4bKkjshdyff"),
    ("3581-4-rport-value", "lower-via", Options, "Via: SIP/2.0/UDP 10.1.1.1:4540;received=192.0.2.1;rport=9988;branch=z9hG4bKkjshdyff"),
    ("3891-6.1-replaces", "", Invite, "Replaces: 425928@bobster.example.org;to-tag=7743;from-tag=6472"),
    ("3891-6.1-replaces-early", "", Invite, "Replaces: 12adf2f34456gs5;to-tag=12345;from-tag=54321;early-only"),
    ("3892-3-referred-by", "", Refer, "Referred-By: <sip:referrer@referrer.example>"),
    ("3892-3-referred-by-cid", "", Refer, "Referred-By: sip:r@ref.example;cid=\"2UWQFN309shb3@ref.example\""),
    ("3892-3-referred-by-compact", "", Refer, "b: <sip:referrer@referrer.example>"),
    ("4028-4-session-expires", "", Invite, "Session-Expires: 4000;refresher=uac"),
    ("4028-4-session-expires-compact", "", Invite, "x: 4000;refresher=uas"),
    ("4028-5-min-se", "", Invite, "Min-SE: 3600"),
    ("4412-3.1-resource-priority", "", Invite, "Resource-Priority: dsn.flash"),
    ("4488-3-refer-sub", "", Refer, "Refer-Sub: false"),
    ("5009-9-p-early-media", "", Response(183, "INVITE"), "P-Early-Media: sendrecv"),
    ("5009-9-p-early-media-list", "", Invite, "P-Early-Media: supported"),
    ("5626-11.1-flow-timer", "", Response(200, "REGISTER"), "Flow-Timer: 126"),
    ("5626-11.2-contact-reg-id", "contact", Register, "Contact: <sip:line1@192.0.2.2;transport=tcp>; reg-id=1\r\n ;+sip.instance=\"<urn:uuid:00000000-0000-1000-8000-000A95A0E128>\""),
    ("5806-3-diversion", "", Invite, "Diversion: <sip:bob@example.com>;reason=unconditional;counter=1;privacy=off"),
    ("6442-4.1-geolocation-cid", "", Invite, "Geolocation: <cid:target123@atlanta.example.com>"),
    ("6442-4.1-geolocation-sips", "", Invite, "Geolocation: <sips:target123@server5.atlanta.example.com>"),
    ("6442-4.2-geolocation-routing", "", Invite, "Geolocation-Routing: no"),
    ("6442-4.3-geolocation-error", "", Response(424, "INVITE"), "Geolocation-Error: 100 ; code=\"Cannot Process Location\""),
    ("6665-8.2.1-event", "event", Subscribe, "Event: foo; id=1234"),
    ("6665-8.2.1-event-compact", "event", Subscribe, "o: foo"),
    ("6665-8.4-allow-events", "", Options, "Allow-Events: foo, bar"),
    ("6665-8.4-allow-events-compact", "", Options, "u: foo, bar"),
    ("6665-8.2.3-subscription-state", "subscription-state", Notify, "Subscription-State: active;expires=60"),
    ("6665-8.2.3-subscription-state-terminated", "subscription-state", Notify, "Subscription-State: terminated;reason=timeout"),
    ("7044-5-history-info", "", Invite, "History-Info: <sip:UserA@ims.example.com>;index=1;foo=bar"),
    ("7044-5-history-info-list", "", Invite, "History-Info: <sip:UserA@ims.example.com?Reason=SIP%3Bcause%3D302>;index=1.1,\r\n <sip:UserB@example.com?Privacy=history&Reason=SIP%3Bcause%3D486>;index=1.2;mp=1.1"),
    ("7315-5.6-charging-vector", "", Invite, "P-Charging-Vector: icid-value=1234bc9876e;\r\n icid-generated-at=192.0.6.8; orig-ioi=home1.net"),
    ("7315-5.4-access-network-info", "", Invite, "P-Access-Network-Info: 3GPP-UTRAN-TDD; utran-cell-id-3gpp=234151D0FCE11"),
    ("7433-4.1-user-to-user", "", Invite, "User-to-User: 56a390f3d2b7310023a2;encoding=hex;purpose=foo;content=bar"),
    ("8760-3-authorization-sha256", "", Register, "Authorization: Digest username=\"bob\", realm=\"biloxi.com\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", uri=\"sip:bob@biloxi.com\", response=\"245f23415f11432b3434341c022\", algorithm=SHA-256"),
];

fn example_row(row: &str, slot: &'static str, base: Base, line: &str) -> Row {
    let template = base.template();
    let template = match slot {
        "" => template.add(line),
        slot => template.with(slot, line),
    };
    let outcome = match read(&template.bytes()) {
        Ok(msg) => check(&msg),
        Err(e) => Err(e.reason),
    };
    Row::new(format!("rfc/{row}"), outcome)
}

fn check(msg: &SipMessage) -> Result<(), String> {
    reads_every_header(msg)
}

/// A message of the RFC's §24 flows: its text, the blank line and body framed
/// by the Content-Length line it states.
fn flow(head: &str, body: &str) -> Vec<u8> {
    let mut out = head.as_bytes().to_vec();
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n{body}", body.len()).as_bytes());
    out
}

/// RFC 3261 §24's registration and session setup, as printed.
fn flows() -> Vec<(&'static str, Vec<u8>)> {
    const SDP: &str = "(Alice's SDP not shown)";
    vec![
        ("3261-24.1-f1-register", flow("REGISTER sip:registrar.biloxi.com SIP/2.0\r\nVia: SIP/2.0/UDP bobspc.biloxi.com:5060;branch=z9hG4bKnashds7\r\nMax-Forwards: 70\r\nTo: Bob <sip:bob@biloxi.com>\r\nFrom: Bob <sip:bob@biloxi.com>;tag=456248\r\nCall-ID: 843817637684230@998sdasdh09\r\nCSeq: 1826 REGISTER\r\nContact: <sip:bob@192.0.2.4>\r\nExpires: 7200\r\n", "")),
        ("3261-24.1-f2-200", flow("SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP bobspc.biloxi.com:5060;branch=z9hG4bKnashds7\r\n ;received=192.0.2.4\r\nTo: Bob <sip:bob@biloxi.com>;tag=2493k59kd\r\nFrom: Bob <sip:bob@biloxi.com>;tag=456248\r\nCall-ID: 843817637684230@998sdasdh09\r\nCSeq: 1826 REGISTER\r\nContact: <sip:bob@192.0.2.4>\r\nExpires: 7200\r\n", "")),
        ("3261-24.2-f1-invite", flow("INVITE sip:bob@biloxi.com SIP/2.0\r\nVia: SIP/2.0/UDP pc33.atlanta.com;branch=z9hG4bKnashds8\r\nMax-Forwards: 70\r\nTo: Bob <sip:bob@biloxi.com>\r\nFrom: Alice <sip:alice@atlanta.com>;tag=1928301774\r\nCall-ID: a84b4c76e66710\r\nCSeq: 314159 INVITE\r\nContact: <sip:alice@pc33.atlanta.com>\r\nContent-Type: application/sdp\r\n", SDP)),
        ("3261-24.2-f2-100", flow("SIP/2.0 100 Trying\r\nVia: SIP/2.0/UDP pc33.atlanta.com;branch=z9hG4bKnashds8\r\n ;received=192.0.2.1\r\nTo: Bob <sip:bob@biloxi.com>\r\nFrom: Alice <sip:alice@atlanta.com>;tag=1928301774\r\nCall-ID: a84b4c76e66710\r\nCSeq: 314159 INVITE\r\n", "")),
        ("3261-24.2-f3-invite", flow("INVITE sip:bob@biloxi.com SIP/2.0\r\nVia: SIP/2.0/UDP bigbox3.site3.atlanta.com;branch=z9hG4bK77ef4c2312983.1\r\nVia: SIP/2.0/UDP pc33.atlanta.com;branch=z9hG4bKnashds8\r\n ;received=192.0.2.1\r\nMax-Forwards: 69\r\nTo: Bob <sip:bob@biloxi.com>\r\nFrom: Alice <sip:alice@atlanta.com>;tag=1928301774\r\nCall-ID: a84b4c76e66710\r\nCSeq: 314159 INVITE\r\nContact: <sip:alice@pc33.atlanta.com>\r\nContent-Type: application/sdp\r\n", SDP)),
        ("3261-24.2-f4-100", flow("SIP/2.0 100 Trying\r\nVia: SIP/2.0/UDP bigbox3.site3.atlanta.com;branch=z9hG4bK77ef4c2312983.1\r\n ;received=192.0.2.2\r\nVia: SIP/2.0/UDP pc33.atlanta.com;branch=z9hG4bKnashds8\r\n ;received=192.0.2.1\r\nTo: Bob <sip:bob@biloxi.com>\r\nFrom: Alice <sip:alice@atlanta.com>;tag=1928301774\r\nCall-ID: a84b4c76e66710\r\nCSeq: 314159 INVITE\r\n", "")),
        ("3261-24.2-f6-180", flow("SIP/2.0 180 Ringing\r\nVia: SIP/2.0/UDP server10.biloxi.com;branch=z9hG4bK4b43c2ff8.1\r\n ;received=192.0.2.3\r\nVia: SIP/2.0/UDP bigbox3.site3.atlanta.com;branch=z9hG4bK77ef4c2312983.1\r\n ;received=192.0.2.2\r\nVia: SIP/2.0/UDP pc33.atlanta.com;branch=z9hG4bKnashds8\r\n ;received=192.0.2.1\r\nTo: Bob <sip:bob@biloxi.com>;tag=a6c85cf\r\nFrom: Alice <sip:alice@atlanta.com>;tag=1928301774\r\nCall-ID: a84b4c76e66710\r\nContact: <sip:bob@192.0.2.4>\r\nCSeq: 314159 INVITE\r\n", "")),
        ("3261-24.2-f12-ack", flow("ACK sip:bob@192.0.2.4 SIP/2.0\r\nVia: SIP/2.0/UDP pc33.atlanta.com;branch=z9hG4bKnashds9\r\nMax-Forwards: 70\r\nTo: Bob <sip:bob@biloxi.com>;tag=a6c85cf\r\nFrom: Alice <sip:alice@atlanta.com>;tag=1928301774\r\nCall-ID: a84b4c76e66710\r\nCSeq: 314159 ACK\r\n", "")),
        ("3261-24.2-f13-bye", flow("BYE sip:alice@pc33.atlanta.com SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.4;branch=z9hG4bKnashds10\r\nMax-Forwards: 70\r\nFrom: Bob <sip:bob@biloxi.com>;tag=a6c85cf\r\nTo: Alice <sip:alice@atlanta.com>;tag=1928301774\r\nCall-ID: a84b4c76e66710\r\nCSeq: 231 BYE\r\n", "")),
    ]
}

/// A fixture of `dir` as the RFC prints it.
fn fixture(dir: &str, name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{dir}/{name}.sip", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// RFC 4475 §3.1.1: the valid messages, read and typed.
const RFC4475_VALID: &[(&str, &str)] = &[
    ("wideRangeValidChars", "3.1.1.2"),
    ("validPercentEscaping", "3.1.1.3"),
    ("escapedNulls", "3.1.1.4"),
    ("percentNotEscape", "3.1.1.5"),
    ("noLwsBeforeAngleBracket", "3.1.1.6"),
    ("extraTrailingOctets", "3.1.1.8"),
    ("semicolonInUserPart", "3.1.1.9"),
    ("variedTransports", "3.1.1.10"),
    ("multipartMime", "3.1.1.11"),
    ("unusualReasonPhrase", "3.1.1.12"),
    ("emptyReasonPhrase", "3.1.1.13"),
];

/// The messages RFC 4475 §3.3 has an element answer with an error of its own:
/// well formed, so they read.
const RFC4475_WELL_FORMED: &[(&str, &str)] = &[
    ("unknownSchemeRequestUri", "3.3.2"),
    ("atypicalSchemeRequestUri", "3.3.3"),
    ("unknownSchemesInHeaders", "3.3.4"),
    ("proxyRequireAndRequire", "3.3.5"),
    ("unknownContentType", "3.3.6"),
    ("unknownAuthorizationScheme", "3.3.7"),
    ("broadcastVia", "3.3.10"),
    ("maxForwardsZero", "3.3.11"),
    ("contactHeaderParam", "3.3.12"),
    ("contactUrlParam", "3.3.13"),
    ("escapedHeaderInContact", "3.3.14"),
    ("unacceptableAccept", "3.3.15"),
];

/// RFC 4475 §3.1.1.1 and §3.1.1.7 carry a top Via with no magic cookie, which
/// ADR-0001 refuses: each is read with that one Via given the cookie, so the rest
/// of the most hostile valid messages meets the parser, byte for byte.
const RFC4475_NO_COOKIE: &[(&str, &str, &[u8], &[u8])] = &[
    ("shortTorturousInvite", "3.1.1.1", b"branch=390skdjuw", b"branch=z9hG4bK390skdjuw"),
    ("longValues", "3.1.1.7", b"sip33.example.com", b"sip33.example.com;branch=z9hG4bK-sip33"),
];

/// `raw` with the first `from` written as `to`.
fn replace_first(raw: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let at = raw.windows(from.len()).position(|w| w == from).expect("the text to replace");
    [&raw[..at], to, &raw[at + from.len()..]].concat()
}

fn message_row(row: String, raw: &[u8]) -> Row {
    let outcome = match read(raw) {
        Ok(msg) => check(&msg),
        Err(e) => Err(e.reason),
    };
    Row::new(row, outcome)
}

/// Every datagram of this table, for a sweep under another parser.
pub fn datagrams() -> Vec<Vec<u8>> {
    let examples = EXAMPLES.iter().map(|&(_, slot, base, line)| match slot {
        "" => base.template().add(line).bytes(),
        slot => base.template().with(slot, line).bytes(),
    });
    let flows = flows().into_iter().map(|(_, raw)| raw);
    let valid = RFC4475_VALID.iter().map(|(name, _)| fixture("rfc4475-valid", name));
    examples.chain(flows).chain(valid).collect()
}

#[test]
fn every_rfc_example_reads() {
    let mut rows: Vec<Row> = EXAMPLES
        .iter()
        .map(|&(row, slot, base, line)| example_row(row, slot, base, line))
        .collect();
    for (row, raw) in flows() {
        rows.push(message_row(format!("rfc/{row}"), &raw));
    }
    for (name, section) in RFC4475_VALID {
        rows.push(message_row(format!("rfc/4475-{section}"), &fixture("rfc4475-valid", name)));
    }
    for (name, section) in RFC4475_WELL_FORMED {
        rows.push(message_row(format!("rfc/4475-{section}"), &fixture("rfc4475-semantics", name)));
    }
    for &(name, section, from, to) in RFC4475_NO_COOKIE {
        let raw = fixture("rfc4475-valid", name);
        assert!(read(&raw).is_err(), "{name}: no magic cookie, ADR-0001");
        let cookied = replace_first(&raw, from, to);
        rows.push(message_row(format!("rfc/4475-{section}-with-cookie"), &cookied));
    }
    settle("rfc", &rows);
}
