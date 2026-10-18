//! The synthetic seeds the lane mutates: the committed RFC 4475 and grammar
//! fixtures, and hand-written messages and descriptions covering every header
//! family and body the read surface types. Addresses are documentation ranges
//! (RFC 5737, RFC 3849) and `example.*` names.

use std::fs;
use std::path::Path;

/// One seed: a stable name (the equivalence digest keys on it) and its bytes.
pub struct Seed {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// The fixture directories read as message seeds.
const FIXTURE_DIRS: &[&str] =
    &["rfc4475-valid", "rfc4475-invalid", "strict-valid", "ipv6", "param-gaps", "cve"];

/// Every message seed: the fixtures, then the synthetic messages.
pub fn messages() -> Vec<Seed> {
    let mut seeds = fixtures();
    seeds.extend(synthetic());
    seeds
}

fn fixtures() -> Vec<Seed> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut seeds = Vec::new();
    for dir in FIXTURE_DIRS {
        let mut entries: Vec<_> = fs::read_dir(root.join(dir))
            .unwrap_or_else(|e| panic!("fixture dir {dir}: {e}"))
            .map(|e| e.expect("fixture entry").path())
            .filter(|p| p.extension().is_some_and(|x| x == "sip"))
            .collect();
        entries.sort();
        for path in entries {
            let stem = path.file_stem().expect("stem").to_string_lossy().into_owned();
            let bytes = fs::read(&path).expect("fixture bytes");
            seeds.push(Seed { name: format!("{dir}/{stem}"), bytes });
        }
    }
    seeds
}

/// A message from its start line, header lines and body, the Content-Length
/// computed and appended last.
fn message(start: &str, headers: &[&str], body: &str) -> Vec<u8> {
    let mut out = String::new();
    out.push_str(start);
    out.push_str("\r\n");
    for h in headers {
        out.push_str(h);
        out.push_str("\r\n");
    }
    out.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    out.push_str(body);
    out.into_bytes()
}

const VIA: &str = "Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-seed-1;rport";
const FROM: &str = "From: \"Alice A\" <sip:+15551230001@example.com;user=phone>;tag=a1";
const TO: &str = "To: <sip:+15551230002@example.net;user=phone>";
const TO_TAGGED: &str = "To: <sip:+15551230002@example.net;user=phone>;tag=b2";
const CALL_ID: &str = "Call-ID: seed-call-1@192.0.2.10";

/// The offer the INVITE seeds carry.
pub const SDP_OFFER: &str = "v=0\r\no=alice 2890844526 2890844526 IN IP4 192.0.2.10\r\ns=-\r\nc=IN IP4 192.0.2.10\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0 8 18 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:18 G729/8000\r\na=fmtp:18 annexb=no\r\na=rtpmap:101 telephone-event/8000\r\na=fmtp:101 0-15\r\na=ptime:20\r\na=sendrecv\r\n";
/// An answer to [`SDP_OFFER`].
pub const SDP_ANSWER: &str = "v=0\r\no=bob 2808844564 2808844564 IN IP4 198.51.100.20\r\ns=-\r\nc=IN IP4 198.51.100.20\r\nt=0 0\r\nm=audio 5004 RTP/AVP 8 101\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:101 telephone-event/8000\r\na=sendrecv\r\n";
/// Two audio streams and a video stream, SDES keys, a session-level direction.
const SDP_STREAMS: &str = "v=0\r\no=- 10 2 IN IP4 203.0.113.5\r\ns=session\r\nc=IN IP4 203.0.113.5\r\nb=AS:128\r\nt=0 0\r\na=sendonly\r\nm=audio 62986 RTP/SAVP 0 4 96\r\na=rtpmap:96 AMR/8000/1\r\na=fmtp:96 octet-align=1\r\na=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:WVNfX19zZW1jdGwgKCkgewkyMjA7fQp9CnVubGVz|2^20|1:4\r\na=crypto:2 AES_CM_128_HMAC_SHA1_32 inline:NzB4d1BINUAvLEw6UzF3WSJ+PSdFcGdUJShpX1Zj\r\nm=audio 0 RTP/AVP 3\r\nm=video 53000 RTP/AVP 32 97\r\nc=IN IP4 203.0.113.6\r\na=rtpmap:97 H264/90000\r\na=fmtp:97 profile-level-id=42e01f\r\na=recvonly\r\n";
/// A held description: unspecified address, inactive.
const SDP_HELD: &str = "v=0\r\no=carol 7 9 IN IP4 0.0.0.0\r\ns=-\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\nm=audio 0 RTP/AVP 0\r\na=inactive\r\n";
/// BUNDLE, ICE and a DTLS setup role.
const SDP_BUNDLE: &str = "v=0\r\no=- 4611731400430051336 2 IN IP6 2001:db8::1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nc=IN IP6 2001:db8::1\r\na=mid:0\r\na=ice-ufrag:abcd\r\na=ice-pwd:0123456789abcdef0123\r\na=setup:actpass\r\na=fingerprint:sha-256 AA:BB:CC\r\na=rtpmap:111 opus/48000/2\r\na=candidate:1 1 UDP 2122260223 2001:db8::1 54400 typ host\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\nc=IN IP6 2001:db8::1\r\na=mid:1\r\na=rtpmap:96 VP8/90000\r\n";

/// The session descriptions the SDP probes mutate on their own.
pub fn descriptions() -> Vec<Seed> {
    [
        ("offer", SDP_OFFER),
        ("answer", SDP_ANSWER),
        ("streams", SDP_STREAMS),
        ("held", SDP_HELD),
        ("bundle", SDP_BUNDLE),
        ("lf-only", "v=0\no=x 1 1 IN IP4 192.0.2.1\ns=-\nc=IN IP4 192.0.2.1\nt=0 0\nm=audio 4000 RTP/AVP 0\na=rtpmap:0 PCMU/8000\n"),
    ]
    .into_iter()
    .map(|(name, text)| Seed { name: format!("sdp/{name}"), bytes: text.as_bytes().to_vec() })
    .collect()
}

/// A multipart body: a description, an XML part and a binary part.
const BOUNDARY: &str = "unique-boundary-1";

fn multipart_body() -> String {
    format!(
        "--{BOUNDARY}\r\nContent-Type: application/sdp\r\nContent-Disposition: session;handling=required\r\n\r\n{SDP_OFFER}\r\n--{BOUNDARY}\r\nContent-Type: application/pidf+xml\r\nContent-ID: <loc@example.com>\r\n\r\n<?xml version=\"1.0\"?><presence/>\r\n--{BOUNDARY}\r\nContent-Type: application/octet-stream\r\nContent-Transfer-Encoding: binary\r\n\r\n\x01\x02\x03\r\n--{BOUNDARY}--\r\n"
    )
}

const MSCML: &str = "<?xml version=\"1.0\"?><MediaServerControl version=\"1.0\"><request><play id=\"332985001\"><prompt><audio url=\"http://example.com/a.wav\"/></prompt></play></request></MediaServerControl>";

fn synthetic() -> Vec<Seed> {
    let invite_headers = [
        VIA,
        "Via: SIP/2.0/TCP [2001:db8::5]:5070;branch=z9hG4bK-seed-0;received=2001:db8::6;rport=5071",
        "Max-Forwards: 70",
        "Route: <sip:proxy.example.com;lr>, <sip:198.51.100.1:5080;lr;transport=tcp>",
        "Record-Route: <sip:rr.example.com;lr;ftag=a1>",
        FROM,
        TO,
        CALL_ID,
        "CSeq: 314159 INVITE",
        "Contact: \"Alice\" <sip:alice@192.0.2.10:5060;transport=udp;ob>;expires=3600;q=0.7;+sip.instance=\"<urn:uuid:00000000-0000-1000-8000-000A95A0E128>\"",
        "Supported: 100rel, timer, replaces",
        "Require: timer",
        "Session-Expires: 1800;refresher=uac",
        "Min-SE: 90",
        "Allow: INVITE, ACK, CANCEL, BYE, PRACK, UPDATE, REFER, NOTIFY, INFO, OPTIONS",
        "Accept: application/sdp, multipart/mixed",
        "P-Asserted-Identity: \"Alice\" <sip:+15551230001@example.com;user=phone>, <tel:+15551230001>",
        "P-Preferred-Identity: <sip:alice@example.com>",
        "Privacy: id;header",
        "Diversion: <sip:+15551230009@example.com>;reason=unconditional;counter=1",
        "History-Info: <sip:bob@example.net?Reason=SIP%3Bcause%3D302>;index=1.1",
        "Remote-Party-ID: \"Alice\" <sip:alice@example.com>;party=calling;privacy=off;screen=no",
        "P-Charging-Vector: icid-value=1234bc9876e;icid-generated-at=192.0.2.10;orig-ioi=home1.example.net",
        "Resource-Priority: wps.3, dsn.flash",
        "X-Trace-Sample: 0.5",
        "Timestamp: 54.21 1.3",
        "User-Agent: seed/1.0",
        "Content-Type: application/sdp",
        "Content-Disposition: session;handling=required",
    ];
    let mut seeds = vec![
        ("invite-sdp", message("INVITE sip:+15551230002@example.net;user=phone SIP/2.0", &invite_headers, SDP_OFFER)),
        (
            "invite-multipart",
            message(
                "INVITE sip:bob@example.net SIP/2.0",
                &[
                    VIA,
                    "Max-Forwards: 70",
                    FROM,
                    TO,
                    CALL_ID,
                    "CSeq: 1 INVITE",
                    "Contact: <sip:alice@192.0.2.10>",
                    "MIME-Version: 1.0",
                    &format!("Content-Type: multipart/mixed;boundary={BOUNDARY}"),
                ],
                &multipart_body(),
            ),
        ),
        (
            "183-reliable",
            message(
                "SIP/2.0 183 Session Progress",
                &[
                    VIA,
                    "Record-Route: <sip:rr.example.com;lr;ftag=a1>",
                    FROM,
                    TO_TAGGED,
                    CALL_ID,
                    "CSeq: 314159 INVITE",
                    "Contact: <sip:bob@198.51.100.20:5060>",
                    "Require: 100rel",
                    "RSeq: 988789",
                    "Content-Type: application/sdp",
                ],
                SDP_ANSWER,
            ),
        ),
        (
            "prack",
            message(
                "PRACK sip:bob@198.51.100.20:5060 SIP/2.0",
                &[VIA, "Max-Forwards: 70", FROM, TO_TAGGED, CALL_ID, "CSeq: 314160 PRACK", "RAck: 988789 314159 INVITE"],
                "",
            ),
        ),
        (
            "200-invite",
            message(
                "SIP/2.0 200 OK",
                &[
                    VIA,
                    "Record-Route: <sip:rr.example.com;lr;ftag=a1>",
                    FROM,
                    TO_TAGGED,
                    CALL_ID,
                    "CSeq: 314159 INVITE",
                    "Contact: <sip:bob@198.51.100.20:5060>;+sip.rendering=\"yes\"",
                    "Session-Expires: 1800;refresher=uac",
                    "Require: timer",
                    "Supported: timer, 100rel",
                    "Allow-Events: refer, dialog",
                    "X-Readiness: new=refuse; cause=backend",
                    "X-Incarnation: 1792155573698",
                    "Content-Type: application/sdp",
                ],
                SDP_ANSWER,
            ),
        ),
        (
            "ack",
            message(
                "ACK sip:bob@198.51.100.20:5060 SIP/2.0",
                &[VIA, "Route: <sip:rr.example.com;lr;ftag=a1>", "Max-Forwards: 70", FROM, TO_TAGGED, CALL_ID, "CSeq: 314159 ACK"],
                "",
            ),
        ),
        (
            "bye-reason",
            message(
                "BYE sip:alice@192.0.2.10:5060 SIP/2.0",
                &[
                    VIA,
                    "Max-Forwards: 70",
                    "From: <sip:+15551230002@example.net;user=phone>;tag=b2",
                    "To: \"Alice A\" <sip:+15551230001@example.com;user=phone>;tag=a1",
                    CALL_ID,
                    "CSeq: 7 BYE",
                    "Reason: Q.850;cause=16;text=\"Normal call clearing\", SIP;cause=200;text=\"Call completed elsewhere\"",
                ],
                "",
            ),
        ),
        (
            "refer",
            message(
                "REFER sip:bob@198.51.100.20 SIP/2.0",
                &[
                    VIA,
                    "Max-Forwards: 70",
                    FROM,
                    TO_TAGGED,
                    CALL_ID,
                    "CSeq: 314161 REFER",
                    "Contact: <sip:alice@192.0.2.10>",
                    "Refer-To: <sip:carol@example.org?Replaces=12345%40192.0.2.4%3Bto-tag%3D12345%3Bfrom-tag%3D5FFE-3994>",
                    "Referred-By: <sip:alice@example.com>",
                    "Refer-Sub: false",
                    "Replaces: 425928@bobster.example.org;to-tag=7743;from-tag=6472",
                ],
                "",
            ),
        ),
        (
            "notify-sipfrag",
            message(
                "NOTIFY sip:alice@192.0.2.10 SIP/2.0",
                &[
                    VIA,
                    "Max-Forwards: 70",
                    "From: <sip:bob@example.net>;tag=b2",
                    "To: <sip:alice@example.com>;tag=a1",
                    CALL_ID,
                    "CSeq: 3 NOTIFY",
                    "Contact: <sip:bob@198.51.100.20>",
                    "Event: refer;id=314161",
                    "Subscription-State: terminated;reason=noresource",
                    "Content-Type: message/sipfrag;version=2.0",
                ],
                "SIP/2.0 200 OK\r\nContact: <sip:carol@example.org>\r\n",
            ),
        ),
        (
            "info-mscml",
            message(
                "INFO sip:ms@203.0.113.9 SIP/2.0",
                &[VIA, "Max-Forwards: 70", FROM, TO_TAGGED, CALL_ID, "CSeq: 9 INFO", "Content-Type: application/mediaservercontrol+xml"],
                MSCML,
            ),
        ),
        (
            "register-auth",
            message(
                "REGISTER sip:registrar.example.com SIP/2.0",
                &[
                    VIA,
                    "Max-Forwards: 70",
                    "From: <sip:alice@example.com>;tag=r1",
                    "To: <sip:alice@example.com>",
                    CALL_ID,
                    "CSeq: 2 REGISTER",
                    "Contact: <sip:alice@192.0.2.10>;expires=600, <sip:alice@[2001:db8::10]:5062>;q=0.1",
                    "Expires: 3600",
                    "Authorization: Digest username=\"alice\", realm=\"example.com\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", uri=\"sip:registrar.example.com\", response=\"6629fae49393a05397450978507c4ef1\", algorithm=MD5, qop=auth, nc=00000001, cnonce=\"0a4f113b\"",
                    "Path: <sip:edge.example.com;lr>",
                ],
                "",
            ),
        ),
        (
            "401-challenge",
            message(
                "SIP/2.0 407 Proxy Authentication Required",
                &[
                    VIA,
                    FROM,
                    TO_TAGGED,
                    CALL_ID,
                    "CSeq: 314159 INVITE",
                    "Proxy-Authenticate: Digest realm=\"example.com\", qop=\"auth,auth-int\", nonce=\"f84f1cec41e6cbe5aea9c8e88d359\", opaque=\"\", stale=FALSE, algorithm=MD5",
                    "WWW-Authenticate: Digest realm=\"example.com\", nonce=\"abc\"",
                ],
                "",
            ),
        ),
        (
            "503-retry",
            message(
                "SIP/2.0 503 Service Unavailable",
                &[
                    VIA,
                    FROM,
                    TO_TAGGED,
                    CALL_ID,
                    "CSeq: 314159 INVITE",
                    "Retry-After: 120 (maintenance);duration=3600",
                    "Warning: 399 proxy.example.com \"overloaded\", 307 198.51.100.1 \"Session parameter 'foo' not understood\"",
                    "Reason: SIP;cause=503;text=\"overload\"",
                    "Unsupported: foo",
                ],
                "",
            ),
        ),
        (
            "options-compact",
            message(
                "OPTIONS sip:carol@example.org SIP/2.0",
                &[
                    "v: SIP/2.0/UDP 192.0.2.10;branch=z9hG4bK-compact",
                    "Max-Forwards: 70",
                    "f: <sip:alice@example.com>;tag=c1",
                    "t: <sip:carol@example.org>",
                    "i: compact-1@192.0.2.10",
                    "CSeq: 1 OPTIONS",
                    "m: <sip:alice@192.0.2.10>",
                    "k: path, gruu",
                    "o: presence",
                    "Accept: application/sdp",
                    "c: application/sdp",
                ],
                "",
            ),
        ),
        (
            "update-timer",
            message(
                "UPDATE sip:bob@198.51.100.20 SIP/2.0",
                &[VIA, "Max-Forwards: 70", FROM, TO_TAGGED, CALL_ID, "CSeq: 314162 UPDATE", "Contact: <sip:alice@192.0.2.10>", "Session-Expires: 4294967295;refresher=uas", "Min-SE: 4294967295", "Supported: timer"],
                "",
            ),
        ),
        (
            "subscribe",
            message(
                "SUBSCRIBE sip:bob@example.net SIP/2.0",
                &[VIA, "Max-Forwards: 70", FROM, TO, CALL_ID, "CSeq: 5 SUBSCRIBE", "Contact: <sip:alice@192.0.2.10>", "Event: dialog;call-id=abc;from-tag=x", "Expires: 0", "Accept: application/dialog-info+xml", "Allow-Events: dialog, presence"],
                "",
            ),
        ),
        (
            "cancel",
            message(
                "CANCEL sip:+15551230002@example.net;user=phone SIP/2.0",
                &[VIA, "Max-Forwards: 70", FROM, TO, CALL_ID, "CSeq: 314159 CANCEL", "Reason: SIP;cause=200;text=\"Call completed elsewhere\""],
                "",
            ),
        ),
    ];
    seeds.push((
        "utf8-display",
        message(
            "MESSAGE sip:zoë@example.com SIP/2.0",
            &[
                VIA,
                "Max-Forwards: 70",
                "From: \"Zoë Ünïcødé €\" <sip:zoë@example.com>;tag=u1",
                "To: \"日本語\" <sip:jp@example.org>",
                CALL_ID,
                "CSeq: 1 MESSAGE",
                "Subject: résumé ✓",
                "Content-Type: text/plain;charset=utf-8",
            ],
            "héllo wörld €",
        ),
    ));
    seeds
        .into_iter()
        .map(|(name, bytes)| Seed { name: format!("synthetic/{name}"), bytes })
        .collect()
}
