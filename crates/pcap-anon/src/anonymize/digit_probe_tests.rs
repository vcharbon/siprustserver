//! The third privacy review's probes, every value invented: digit runs in the
//! opaque ids (Call-ID local part, tag, branch), in charging ids, vendor
//! headers, free text, SDP attributes, sipfrag headers, control XML and MIME
//! entity headers; addresses inside ids; names under a `sip` first label;
//! non-ASCII in a vendor header and in a control body. Each probe must leave
//! none of them, keep every datagram's length and parse, and the leak checks
//! must count nothing; untouched, they count something. Peer addresses are
//! public-looking (85.12.34.x, a global IPv6) on purpose: the scope maps
//! only public addresses.

use serde_json::json;

use crate::anonymize::{anonymize_in, parse_ok_map, Fakes};
use crate::fake::address::Scope;
use crate::fake::Seed;
use crate::model::{ingest_str, Flows};

const HEAD: &str = "INVITE sip:+2861112222@callee-dom.de SIP/2.0";

/// The probe's header lines.
fn head(variant: char) -> Vec<&'static str> {
    match variant {
        'a' => vec![
            "Via: SIP/2.0/UDP 85.12.34.10:5060;branch=z9hG4bK2861110003",
            "From: \"Fakey McFakename\" <sip:+2861113333@caller-dom.de>;tag=2861110002",
            "To: <sip:+2861112222@callee-dom.de>",
            "Call-ID: 2861110001-77ab@cidhost.callerdom.zz",
            "CSeq: 2 INVITE",
            "Max-Forwards: 70",
            "Contact: <sip:+2861113333@85.12.34.10:5060>;+sip.instance=\"<urn:gsma:imei:35000099-176148-1>\"",
            "P-Charging-Vector: icid-value=2861110004;orig-ioi=2861110005",
            "X-Called: 2861110006",
            "X-Caller-Name: \"Testo Placeholder\"",
            "Subject: 2861110007",
        ],
        'b' => vec![
            "Via: SIP/2.0/UDP 85.12.34.10:5060;branch=z9hG4bK.85.12.34.71",
            // A tag is a token, a Call-ID word admits `:` (RFC 3261 §25.1).
            "From: <sip:+2861113333@caller-dom.de>;tag=85.12.34.73",
            "To: <sip:+2861112222@callee-dom.de>",
            "Call-ID: x-3fff:e0a:1:2::72-77ab@cidhost.callerdom.zz",
            "CSeq: 2 INVITE",
            "Max-Forwards: 70",
            "Contact: <sip:+2861113333@85.12.34.10:5060>",
            "X-Hop: sip.hidden-xhop.de",
            "Record-Route: <sip:sip.rr-hidden.de;lr>",
            "X-Note: \"see http://sip.note-hidden.de/x\"",
        ],
        'c' => vec![
            "Via: SIP/2.0/UDP 85.12.34.10:5060;branch=z9hG4bKc1",
            "From: \"Fakey 061110020\" <sip:+2861113333@caller-dom.de>;tag=c2",
            "To: <sip:+2861112222@callee-dom.de>",
            "Call-ID: c3@cidhost.callerdom.zz",
            "CSeq: 2 INVITE",
            "Max-Forwards: 70",
            "Contact: <sip:+2861113333@85.12.34.10:5060>",
            "Identity: eyJhbGciOiJFUzI1NiJ9.eyJvcmlnIjp7InRuIjoiMjg2MTExMzMzMyJ9fQ.c2lnbmF0dXJl;info=<https://cert.fakesti-c.de/sti.pem>;alg=ES256",
            "X-Who: Fäkey",
        ],
        'd' => vec![
            "Via: SIP/2.0/UDP 85.12.34.10:5060;branch=z9hG4bK2861113333x",
            "From: <sip:+2861113333@caller-dom.de>;tag=9061113333",
            "To: <sip:+2861112222@callee-dom.de>",
            "Call-ID: 0611133337771@cidhost.callerdom.zz",
            "CSeq: 2 INVITE",
            "Max-Forwards: 70",
            "Contact: <sip:+2861113333@85.12.34.10:5060>",
            "X-Orig: 002861112222",
        ],
        _ => vec![
            "Via: SIP/2.0/UDP 85.12.34.10:5060;branch=z9hG4bKe1",
            "From: <sip:+2861113333@caller-dom.de>;tag=e2",
            "To: <sip:+2861112222@callee-dom.de>",
            "Call-ID: e3@cidhost.callerdom.zz",
            "CSeq: 2 INVITE",
        ],
    }
}

/// The probe's body and its content type.
fn body(variant: char) -> (&'static str, &'static str) {
    match variant {
        'a' => (
            "multipart/mixed;boundary=b1",
            "--b1\r\nContent-Type: application/sdp\r\n\r\n\
             v=0\r\no=- 1 1 IN IP4 85.12.34.10\r\ns=-\r\nc=IN IP4 85.12.34.10\r\nt=0 0\r\n\
             m=audio 4000 RTP/AVP 0\r\na=x-ref:http://media.sdp-hidden.de/a\r\n\
             a=x-host:sip.sdp-hidden2.de\r\na=x-num:2861110012\r\na=x-name:Dummy Notaperson\r\n\
             --b1\r\nContent-Type: message/sipfrag\r\n\r\n\
             SIP/2.0 200 OK\r\nFrom: \"Fakey Fakerson\" <sip:+2861114444@frag-dom.de>;tag=2861110013\r\n\
             --b1\r\nContent-Type: application/mediaservercontrol+xml\r\n\r\n\
             <mscivr><dialogstart connectionid=\"2861110014~abc\"><dialog><prompt>\
             <media loc=\"http://ann.ivrx-dom.zz/x.wav\"/></prompt><var name=\"caller\" value=\"2861110015\"/>\
             <host name=\"sip.ivrx-hidden.zz\"/><who>Nobody Testname</who></dialog></dialogstart></mscivr>\r\n\
             --b1--\r\n",
        ),
        'c' => (
            "multipart/mixed;boundary=b1",
            "--b1\r\nContent-Type: application/sdp\r\n\r\n\
             v=0\r\no=- 1 1 IN IP4 85.12.34.10\r\ns=-\r\nc=IN IP4 85.12.34.10\r\nt=0 0\r\n\
             m=audio 4000 RTP/AVP 0\r\na=x-host:media.sdp-plain-c.de\r\n\
             --b1\r\nContent-Type: application/pidf+xml\r\n\r\n\
             <presence>\r\n<RD>Rue Fakestreet</RD>\r\n<HNO>12</HNO>\r\n</presence>\r\n\
             --b1\r\nContent-Type: application/mediaservercontrol+xml\r\n\r\n\
             <mscivr><who>Fäkey</who><host name=\"media.ivrx-plain-c.zz\"/></mscivr>\r\n\
             --b1--\r\n",
        ),
        'e' => ("application/sdp", concat!("v=0\r\no=- 1 1 IN IP4 85.12.34.10\r\ns=-\r\nc=IN IP4 85.12.34.10\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\n", "a=x-host:fcb.bc\r\n")),
        'f' => ("application/sdp", concat!("v=0\r\no=- 1 1 IN IP4 85.12.34.10\r\ns=-\r\nc=IN IP4 85.12.34.10\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\n", "a=x-host:fcz.bc\r\n")),
        'g' => (
            "multipart/mixed;boundary=b1",
            "--b1\r\nContent-Type: application/pidf+xml\r\n\
             Content-Location: <http://geo.loc-hidden-g.de/l>\r\n\
             X-Part-Note: sip.part-hidden-g.de 2861110030\r\n\r\n<presence/>\r\n--b1--\r\n",
        ),
        _ => ("", ""),
    }
}

/// The identities each probe carries, none of which may survive.
fn identities(variant: char) -> &'static [&'static str] {
    match variant {
        'a' => &[
            "61110001",
            "61110002",
            "61110003",
            "61110004",
            "61110005",
            "61110006",
            "Testo",
            "Placeholder",
            "61110007",
            "sdp-hidden",
            "61110012",
            "Dummy",
            "Notaperson",
            "Fakey",
            "Fakerson",
            "61110013",
            "61110014",
            "61110015",
            "ivrx-hidden",
            "ivrx-dom",
            "Nobody",
            "Testname",
            "callee-dom",
            "caller-dom",
            "callerdom",
            "frag-dom",
        ],
        'b' => &[
            "85.12.34.71",
            "3fff:e0a:1:2::72",
            "85.12.34.73",
            "hidden-xhop",
            "rr-hidden",
            "note-hidden",
        ],
        'c' => {
            &["061110020", "fakesti-c", "Fäkey", "Rue Fakestreet", "sdp-plain-c", "ivrx-plain-c"]
        }
        'd' => &["61113333", "61112222", "7771"],
        'e' => &["fcb.bc"],
        'f' => &["fcz.bc"],
        _ => &["loc-hidden-g", "part-hidden-g", "61110030"],
    }
}

fn probe(variant: char) -> Flows {
    let (content_type, body) = body(variant);
    let mut lines = vec![HEAD];
    lines.extend(head(variant));
    let typed = format!("Content-Type: {content_type}");
    let length = format!("Content-Length: {}", body.len());
    if !content_type.is_empty() {
        lines.push(&typed);
    }
    lines.push(&length);
    let raw = format!("{}\r\n\r\n{body}", lines.join("\r\n"));
    let call_id = lines.iter().find_map(|l| l.strip_prefix("Call-ID: ")).expect("a Call-ID");
    let doc = json!({
        "schema": crate::model::SUPPORTED_SCHEMA,
        "decode_stats": { "records": 0, "non_ip": 0, "non_udp": 0, "snap_truncated": 0, "datagrams": 0,
                          "fragments": 0, "reassembled": 0, "frag_dropped": 0, "tail_truncated": 0 },
        "flow_stats": { "sip_messages": 1, "capture_dups": 0, "parse_failed": 0, "non_sip": 0 },
        "legs": [{ "call_id": call_id, "hops": [{ "a": "85.12.34.10:5060", "b": "172.16.245.246:5060" }],
                   "invite": null, "final_status": null, "saw_180": false, "terminated_by": null, "tokens": [],
                   "msgs": [{ "ts_us": 0, "src": "85.12.34.10:5060", "dst": "172.16.245.246:5060", "hop": 0,
                              "retx": false, "raw": raw,
                              "summary": { "kind": "request", "method": "INVITE",
                                           "uri": "sip:+2861112222@callee-dom.de",
                                           "cseq": { "seq": 2, "method": "INVITE" },
                                           "from": { "uri": "sip:+2861113333@caller-dom.de", "tag": null },
                                           "to": { "uri": "sip:+2861112222@callee-dom.de", "tag": null } } }] }],
        "groups": []
    });
    ingest_str(&doc.to_string()).expect("the probe ingests")
}

fn wire(flows: &Flows) -> String {
    String::from_utf8_lossy(&crate::model::wire(&flows.legs[0].msgs[0])).into_owned()
}

/// Every leak count of `--verify` over one anonymisation, summed.
fn leaks(raw: &Flows, out: &Flows, fakes: &Fakes) -> usize {
    let (plan, scope) = (crate::testflow::plan(), Scope::default());
    crate::verify::survivors(raw, out, &plan, &scope)
        + crate::verify::unmasked_displays(out, &scope)
        + crate::verify::names::name_survivors(raw, out, &scope)
        + crate::verify::leaks::leaks(out, &scope, fakes, &crate::testflow::reading()).total()
}

const VARIANTS: [char; 7] = ['a', 'b', 'c', 'd', 'e', 'f', 'g'];

#[test]
fn every_identity_of_every_probe_is_gone_and_verify_counts_nothing() {
    for variant in VARIANTS {
        let raw = probe(variant);
        let mut out = probe(variant);
        let parsed = parse_ok_map(&out, &crate::testflow::reading());
        let report = anonymize_in(
            &mut out,
            &crate::testflow::plan(),
            &Seed::from_bytes([5; 32]),
            &Scope::default(),
        )
        .expect("anonymises");
        let text = wire(&out).to_lowercase();
        for id in identities(variant) {
            assert!(
                !text.contains(&id.to_lowercase()),
                "probe {variant}: {id} survives:\n{}",
                wire(&out)
            );
        }
        assert_eq!(
            crate::model::wire(&raw.legs[0].msgs[0]).len(),
            crate::model::wire(&out.legs[0].msgs[0]).len(),
            "probe {variant}: length"
        );
        assert_eq!(
            parse_ok_map(&out, &crate::testflow::reading()),
            parsed,
            "probe {variant}: still parses"
        );
        assert_eq!(
            leaks(&raw, &out, &report.fakes),
            0,
            "probe {variant}: verify counts a leak:\n{}",
            wire(&out)
        );
        assert!(text.contains(":5060"), "probe {variant}: ports are never faked");
    }
}

#[test]
fn verify_counts_what_a_probe_leaves_when_nothing_anonymises_it() {
    for variant in VARIANTS {
        let raw = probe(variant);
        let digits = crate::verify::leaks::leaks(
            &raw,
            &Scope::default(),
            &Fakes::default(),
            &crate::testflow::reading(),
        );
        assert!(leaks(&raw, &probe(variant), &Fakes::default()) > 0, "probe {variant}");
        if matches!(variant, 'a' | 'd' | 'g') {
            assert!(digits.digits > 0, "probe {variant}: {digits:?}");
        }
    }
}
