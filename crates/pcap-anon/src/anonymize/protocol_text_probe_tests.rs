//! Protocol text is never anonymised: a status line's reason phrase (a
//! response's, a sipfrag's), a Reason's text, a Warning's text and a
//! control-protocol attribute's value stay byte for byte whatever they hold,
//! because of where they sit, and verify reads none of them. The same name or
//! number anywhere else is still mapped and counted. One probe per site,
//! every value invented. The peer addresses are public-looking (85.12.34.x)
//! on purpose: the scope maps only public addresses.

use super::refusal_probe_tests::{document, leaks, wire};
use crate::anonymize::{anonymize_in, Fakes};
use crate::fake::address::Scope;
use crate::fake::Seed;
use crate::model::{ingest_str, Flows};

/// One site: its document, the protocol text kept byte for byte, and the
/// identities outside it that go.
struct Site {
    name: &'static str,
    doc: Flows,
    kept: &'static [&'static str],
    gone: &'static [&'static str],
}

/// A one-response document carrying `raw`.
pub(super) fn response(raw: &str) -> Flows {
    let doc = serde_json::json!({
        "schema": crate::model::SUPPORTED_SCHEMA,
        "decode_stats": { "records": 0, "non_ip": 0, "non_udp": 0, "snap_truncated": 0, "datagrams": 0,
                          "fragments": 0, "reassembled": 0, "frag_dropped": 0, "tail_truncated": 0 },
        "flow_stats": { "sip_messages": 1, "capture_dups": 0, "parse_failed": 0, "non_sip": 0 },
        "legs": [{ "call_id": "c1@cidhost.callerdom.zz", "hops": [{ "a": "85.12.34.10:5060", "b": "172.16.245.246:5060" }],
                   "invite": null, "final_status": null, "saw_180": false, "terminated_by": null, "tokens": [],
                   "msgs": [{ "ts_us": 0, "src": "172.16.245.246:5060", "dst": "85.12.34.10:5060", "hop": 0,
                              "retx": false, "raw": raw,
                              "summary": { "kind": "response", "status": 487, "reason": "Request Fakey McFakename",
                                           "cseq": { "seq": 2, "method": "INVITE" },
                                           "from": { "uri": "sip:+2861113333@caller-dom.de", "tag": "p2" },
                                           "to": { "uri": "sip:+2861112222@callee-dom.de", "tag": "p3" } } }] }],
        "groups": []
    });
    ingest_str(&doc.to_string()).expect("the probe ingests")
}

/// `doc` with its first datagram's body typed `media`.
fn typed(doc: Flows, media: &str) -> Flows {
    let mut d = doc;
    let text = wire(&d).replace("application/sdp", media);
    crate::model::set_wire(&mut d.legs[0].msgs[0], text.as_bytes());
    d
}

fn sites() -> Vec<Site> {
    let call = "c1@cidhost.callerdom.zz";
    let status = "SIP/2.0 487 Request Fakey McFakename +2862345678 85.12.34.66 fake-sbc.notreal-carrier.de\r\n\
                  Via: SIP/2.0/UDP 85.12.34.10:5060;branch=z9hG4bKp1\r\n\
                  From: <sip:+2861113333@caller-dom.de>;tag=p2\r\n\
                  To: <sip:+2861112222@callee-dom.de>;tag=p3\r\n\
                  Call-ID: c1@cidhost.callerdom.zz\r\nCSeq: 2 INVITE\r\nContent-Length: 0\r\n\r\n";
    vec![
        Site {
            name: "status line reason phrase",
            doc: response(status),
            kept: &["SIP/2.0 487 Request Fakey McFakename +2862345678 85.12.34.66 fake-sbc.notreal-carrier.de\r\n"],
            gone: &["61113333", "caller-dom"],
        },
        Site {
            name: "sipfrag reason phrase",
            doc: typed(
                document(&[], call, "SIP/2.0 503 Fakey McFakename at +2862345678 fake-sbc.notreal-carrier.de\r\n"),
                "message/sipfrag",
            ),
            kept: &["\r\nSIP/2.0 503 Fakey McFakename at +2862345678 fake-sbc.notreal-carrier.de\r\n"],
            gone: &["61113333", "caller-dom"],
        },
        Site {
            name: "Q.850 Reason text",
            doc: document(&["Reason: Q.850;cause=16;text=\"call to +2862345678 cleared\""], call, ""),
            kept: &["Reason: Q.850;cause=16;text=\"call to +2862345678 cleared\"\r\n"],
            gone: &["61113333", "caller-dom"],
        },
        Site {
            name: "SIP Reason text",
            doc: document(
                &["Reason: SIP;cause=487;text=\"Fakey McFakename at fake-sbc.notreal-carrier.de, 85.12.34.66\""],
                call,
                "",
            ),
            kept: &["text=\"Fakey McFakename at fake-sbc.notreal-carrier.de, 85.12.34.66\"\r\n"],
            gone: &["61113333", "caller-dom"],
        },
        Site {
            // The warn-agent is a field and maps; only the text stays.
            name: "Warning text",
            doc: document(
                &["Warning: 399 fake-sbc.notreal-carrier.de \"Fakey McFakename +2862345678\""],
                call,
                "",
            ),
            kept: &[" \"Fakey McFakename +2862345678\"\r\n"],
            gone: &["399 fake-sbc.notreal-carrier.de", "61113333"],
        },
        Site {
            name: "control-protocol attribute values",
            doc: typed(
                document(
                    &[],
                    call,
                    "<?xml version=\"1.0\"?><MediaServerControl version=\"1.0\"><response \
                     request=\"play\" code=\"200\" reason=\"Fakey McFakename\" text=\"OK via \
                     fake-sbc.notreal-carrier.de +2862345678\"/><var name=\"who\" \
                     value=\"Gertrude Fakenstein\"/></MediaServerControl>",
                ),
                "application/mediaservercontrol+xml",
            ),
            kept: &["reason=\"Fakey McFakename\" text=\"OK via fake-sbc.notreal-carrier.de +2862345678\""],
            gone: &["Fakenstein", "61113333"],
        },
    ]
}

#[test]
fn protocol_text_stays_byte_for_byte_and_verify_counts_nothing_in_it() {
    for site in sites() {
        let mut out = site.doc.clone();
        let report = anonymize_in(
            &mut out,
            &crate::testflow::plan(),
            &Seed::from_bytes([11; 32]),
            &Scope::default(),
        )
        .expect("anonymises");
        let text = wire(&out);
        for kept in site.kept {
            assert!(text.contains(kept), "{}: {kept:?} lost:\n{text}", site.name);
        }
        for gone in site.gone {
            assert!(!text.contains(gone), "{}: {gone} survives:\n{text}", site.name);
        }
        assert_eq!(
            leaks(&site.doc, &out, &report.fakes),
            0,
            "{}: verify counts:\n{text}",
            site.name
        );
    }
}

#[test]
fn a_response_keeps_its_reason_phrase_in_the_model_too() {
    let mut out = response(
        "SIP/2.0 487 Request Fakey McFakename\r\nVia: SIP/2.0/UDP 85.12.34.10:5060;branch=z9hG4bKp1\r\n\
         From: <sip:+2861113333@caller-dom.de>;tag=p2\r\nTo: <sip:+2861112222@callee-dom.de>;tag=p3\r\n\
         Call-ID: c1@cidhost.callerdom.zz\r\nCSeq: 2 INVITE\r\nContent-Length: 0\r\n\r\n",
    );
    anonymize_in(
        &mut out,
        &crate::testflow::plan(),
        &Seed::from_bytes([11; 32]),
        &Scope::default(),
    )
    .expect("anonymises");
    let model = serde_json::to_string(&out.legs[0].msgs[0].summary).expect("serialises");
    assert!(model.contains("\"Request Fakey McFakename\""), "{model}");
}

#[test]
fn the_same_name_and_number_outside_protocol_text_are_mapped_and_counted() {
    // The number and the name stand in a Reason's text and in an identity
    // header and a free-text header: mapped there, kept in the text.
    let headers = [
        "Reason: Q.850;cause=16;text=\"call to +2862345678 by Fakey McFakename cleared\"",
        "P-Asserted-Identity: <sip:+2862345678@fake-sbc.notreal-carrier.de>",
        "Subject: Fakey McFakename",
    ];
    let doc = document(&headers, "c1@cidhost.callerdom.zz", "");
    let mut out = doc.clone();
    let report = anonymize_in(
        &mut out,
        &crate::testflow::plan(),
        &Seed::from_bytes([11; 32]),
        &Scope::default(),
    )
    .expect("anonymises");
    let text = wire(&out);
    assert!(text.contains("text=\"call to +2862345678 by Fakey McFakename cleared\""), "{text}");
    assert_eq!(text.matches("62345678").count(), 1, "mapped outside the text:\n{text}");
    assert_eq!(text.matches("Fakey McFakename").count(), 1, "mapped outside the text:\n{text}");
    assert!(!text.contains("@fake-sbc.notreal-carrier.de"), "{text}");
    assert_eq!(leaks(&doc, &out, &report.fakes), 0, "{text}");
    // Left in place, the same identities count outside the text and not in it.
    let none = Fakes::default();
    let inside_only = document(&headers[..1], "c1@cidhost.callerdom.zz", "");
    let bare = document(
        &["Reason: Q.850;cause=16;text=\"call to +abcdefghijk by aaaaa bbbbbbbbbb cleared\""],
        "c1@cidhost.callerdom.zz",
        "",
    );
    let floor = leaks(&bare, &bare, &none);
    assert_eq!(leaks(&inside_only, &inside_only, &none), floor, "verify reads no protocol text");
    assert!(leaks(&doc, &doc, &none) > floor, "verify counts them outside protocol text");
}
