//! A number carried base64-encoded, in a URI parameter or in a header value,
//! in either alphabet, padded or not: the rewrite maps the number its decoded
//! text holds to the fake it takes everywhere and re-encodes, and the leak
//! check decodes the same runs. A run whose decoding is no text (random
//! bytes that spell digits by accident, a control byte among ASCII) or holds
//! nothing a pass maps stays as written. Every value is invented.

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::engine::GeneralPurpose;
use base64::Engine as _;
use serde_json::json;

use crate::anonymize::anonymize_in;
use crate::fake::address::Scope;
use crate::fake::Seed;
use crate::model::{ingest_str, Flows};

/// One BYE from `from` (a SIP URI user) carrying the `extra` header lines.
pub(super) fn doc(from: &str, extra: &[String]) -> Flows {
    doc_of(from, "enc-call", extra, None)
}

/// One BYE from `from` (a SIP URI user) of the Call-ID `call` at the SBC's
/// host, carrying the `extra` header lines and `body` (its media type, then
/// its content).
pub(super) fn doc_of(
    from: &str,
    call: &str,
    extra: &[String],
    body: Option<(&str, &str)>,
) -> Flows {
    let uri = "sip:testo@fake-sbc.example.com";
    let from_uri = format!("sip:{from}@fake-sbc.example.com");
    let call_id = format!("{call}@fake-sbc.example.com");
    let lines: String = extra.iter().map(|line| format!("{line}\r\n")).collect();
    let (content_type, content) = match body {
        Some((media, content)) => (format!("Content-Type: {media}\r\n"), content),
        None => (String::new(), ""),
    };
    let raw = format!(
        "BYE {uri} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bKenc1\r\n\
         From: <{from_uri}>;tag=encf\r\n\
         To: <{uri}>;tag=enct\r\n\
         Call-ID: {call_id}\r\nCSeq: 2 BYE\r\nMax-Forwards: 70\r\n\
         {lines}{content_type}\
         Content-Length: {}\r\n\r\n{content}",
        content.len()
    );
    let doc = json!({
        "schema": crate::model::SUPPORTED_SCHEMA,
        "decode_stats": { "records": 0, "non_ip": 0, "non_udp": 0, "snap_truncated": 0, "datagrams": 0,
                          "fragments": 0, "reassembled": 0, "frag_dropped": 0, "tail_truncated": 0 },
        "flow_stats": { "sip_messages": 1, "capture_dups": 0, "parse_failed": 0, "non_sip": 0 },
        "legs": [{ "call_id": call_id,
                   "hops": [{ "a": "192.0.2.10:5060", "b": "198.51.100.20:5060" }],
                   "invite": null, "final_status": null, "saw_180": false, "terminated_by": null, "tokens": [],
                   "msgs": [{ "ts_us": 0, "src": "192.0.2.10:5060", "dst": "198.51.100.20:5060", "hop": 0,
                              "retx": false, "raw": raw,
                              "summary": { "kind": "request", "method": "BYE", "uri": uri,
                                           "cseq": { "seq": 2, "method": "BYE" },
                                           "from": { "uri": from_uri, "tag": "encf" },
                                           "to": { "uri": uri, "tag": "enct" } } }] }],
        "groups": []
    });
    ingest_str(&doc.to_string()).expect("the probe ingests")
}

fn seed() -> Seed {
    Seed::from_bytes([5; 32])
}

pub(super) fn wire(flows: &Flows) -> String {
    String::from_utf8_lossy(&crate::model::wire(&flows.legs[0].msgs[0])).into_owned()
}

/// The value written behind `open` on the wire, up to the first of `close`.
pub(super) fn between(wire: &str, open: &str, close: &[char]) -> String {
    let at = wire.find(open).expect("the probe's line") + open.len();
    wire[at..].split(close).next().expect("a closing byte").to_string()
}

/// Anonymise `raw` under the test plan and the default scope: the output
/// and the verdict total.
pub(super) fn anonymised(raw: &Flows) -> (Flows, usize) {
    let (out, verdict, _) = anonymised_with_report(raw);
    (out, verdict)
}

/// [`anonymised`], with the run's report.
pub(super) fn anonymised_with_report(raw: &Flows) -> (Flows, usize, crate::anonymize::Report) {
    let (plan, scope) = (crate::testflow::plan(), Scope::default());
    let mut out = raw.clone();
    let report = anonymize_in(&mut out, &plan, &seed(), &scope).expect("anonymises");
    let verdict = crate::verify::verdict::judge(
        raw,
        &out,
        &plan,
        &scope,
        &crate::testflow::reading(),
        &report,
    )
    .total();
    (out, verdict, report)
}

const ENGINES: [(&str, GeneralPurpose); 4] = [
    ("standard", STANDARD),
    ("standard unpadded", STANDARD_NO_PAD),
    ("url-safe", URL_SAFE),
    ("url-safe unpadded", URL_SAFE_NO_PAD),
];

#[test]
fn a_base64_number_in_a_uri_parameter_takes_its_fake_and_is_re_encoded() {
    let number = "+15556345678";
    for (alphabet, engine) in ENGINES {
        let encoded = engine.encode(number);
        let raw = doc(number, &[format!("X-Ref: <sip:ref@ref-host.example.com;ident={encoded}>")]);
        let (out, verdict) = anonymised(&raw);
        let wire = wire(&out);
        let written = between(&wire, ";ident=", &['>']);
        assert_eq!(written.len(), encoded.len(), "{alphabet}: the run keeps its width");
        let decoded = engine.decode(&written).expect("the run is still base64 of its alphabet");
        let fake_user = between(&wire, "From: <sip:", &['@']);
        assert_ne!(fake_user, number, "{alphabet}: the From user is mapped");
        assert_eq!(
            String::from_utf8(decoded).expect("text"),
            fake_user,
            "{alphabet}: the encoded number takes the fake the From user takes"
        );
        assert_eq!(verdict, 0, "{alphabet}: the leak check counts nothing");
    }
}

#[test]
fn a_base64_number_in_a_header_value_is_mapped_where_it_stands_in_the_text() {
    let text = "caller=+15557778888;line=2";
    for (alphabet, engine) in ENGINES {
        let encoded = engine.encode(text);
        let raw = doc("fakey", &[format!("X-Caller-Token: {encoded}")]);
        let (out, verdict) = anonymised(&raw);
        let written = between(&wire(&out), "X-Caller-Token: ", &['\r']);
        assert_eq!(written.len(), encoded.len(), "{alphabet}: the run keeps its width");
        let decoded = engine.decode(&written).expect("the run is still base64 of its alphabet");
        let decoded = String::from_utf8(decoded).expect("text");
        let digits = |s: &str| s.chars().filter(char::is_ascii_digit).collect::<String>();
        let letters = |s: &str| s.chars().filter(|c| !c.is_ascii_digit()).collect::<String>();
        assert_eq!(letters(&decoded), letters(text), "{alphabet}: {decoded}: the rest is kept");
        assert!(!decoded.contains("5557778888"), "{alphabet}: {decoded}: the number is mapped");
        assert_ne!(digits(&decoded), digits(text), "{alphabet}: {decoded}");
        assert_eq!(verdict, 0, "{alphabet}: the leak check counts nothing");
    }
}

#[test]
fn verify_decodes_a_base64_number_left_in_place() {
    let (plan, scope) = (crate::testflow::plan(), Scope::default());
    for (alphabet, engine) in ENGINES {
        let encoded = engine.encode("caller=+15557778888");
        let raw = doc(
            "fakey",
            &[
                format!(
                    "X-Ref: <sip:ref@ref-host.example.com;ident={}>",
                    engine.encode("+15556345678")
                ),
                format!("X-Caller-Token: {encoded}"),
            ],
        );
        assert_eq!(
            crate::verify::survivors(&raw, &raw, &plan, &scope),
            2,
            "{alphabet}: the two encoded numbers survive"
        );
        let unexplained = crate::verify::leaks::unexplained(
            &raw,
            &scope,
            &crate::anonymize::Fakes::default(),
            &crate::testflow::reading(),
        );
        for number in ["15556345678", "15557778888"] {
            assert!(
                unexplained.contains(&("digits", number.as_bytes().to_vec())),
                "{alphabet}: the leak check reads {number} decoded"
            );
        }
    }
}

#[test]
fn a_run_that_decodes_to_no_text_or_to_no_number_is_no_identity() {
    let random =
        [0x9c, 0x01, b'5', b'5', b'5', b'6', b'3', b'4', b'5', b'6', b'7', 0xfe, 0x00, 0x7f];
    let ascii_with_control_bytes = b"\x0115556345678\x7f";
    let tokens = [
        STANDARD.encode(random),
        URL_SAFE_NO_PAD.encode(random),
        STANDARD.encode(ascii_with_control_bytes),
        STANDARD.encode("session-abc-xyz"),
        "Abcdefgh1234".to_string(),
    ];
    for token in tokens {
        let raw = doc("fakey", &[format!("X-Nonce: {token}")]);
        let (out, verdict) = anonymised(&raw);
        assert_eq!(between(&wire(&out), "X-Nonce: ", &['\r']), token, "{token}: left as written");
        assert_eq!(verdict, 0, "{token}: the leak check counts nothing");
    }
}
