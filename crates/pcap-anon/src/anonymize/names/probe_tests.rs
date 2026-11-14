//! The second privacy review's probes, every value invented: identities
//! written where the first passes did not look (escaped URIs, tel visual
//! separators, non-numeric user parts, instance ids, cell ids, free text,
//! SDP lines, multipart bodies, PASSporTs, base64 parameters, ports), and
//! real values that a shape test would have taken for fakes. Each probe must
//! leave none of them, keep every datagram's length and parse, and the
//! leak checks must count nothing. Peer addresses are public-looking
//! (85.12.34.x) on purpose: the scope maps only public addresses.

use base64::Engine as _;
use serde_json::json;

use crate::anonymize::{anonymize_in, parse_ok_map, AnonymizeError};
use crate::fake::address::Scope;
use crate::fake::Seed;
use crate::model::{ingest_str, Flows};
use crate::plan::Plan;

fn b64u(s: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s)
}

/// The probe's extra header lines.
fn extra(variant: char) -> Vec<String> {
    let jws = format!(
        "{}.{}.{}",
        b64u(
            r#"{"alg":"ES256","ppt":"shaken","typ":"passport","x5u":"https://cert.fakecorp-certs.de/sti.pem"}"#
        ),
        b64u(
            r#"{"attest":"A","dest":{"tn":["2868765432"]},"iat":1700000000,"orig":{"tn":"2862345678"},"origid":"0f6c2a1e-1b2c-4d5e-8f90-a1b2c3d4e5f6"}"#
        ),
        b64u("signature-bytes-invented-0123456789")
    );
    let b64num = base64::engine::general_purpose::STANDARD.encode("+2867778888");
    let lines: Vec<String> = match variant {
        'a' => vec![
            "Contact: <sip:fakey.mcfakename@85.12.34.201;transport=udp>;+sip.instance=\"<urn:gsma:imei:35000099-176148-1>\";expires=600".into(),
            "P-Preferred-Identity: <sip:testo.placeholder@prefid-domain.de>".into(),
            "P-Access-Network-Info: 3GPP-E-UTRAN-FDD; utran-cell-id-3gpp=001019876ABCDE1F".into(),
            format!("Identity: {jws};info=<https://cert.fakecorp-certs.de/sti.pem>;alg=ES256;ppt=shaken"),
            "Geolocation: <cid:loc1@geo-cid-domain.de>, <https://lis.geo-lis-domain.de/loc/42>".into(),
            "X-Note: \"reached 85.12.34.199 via jump\"".into(),
            "P-Charging-Vector: icid-value=abcd1234ef;orig-ioi=Fakenetone;term-ioi=Fakenettwo".into(),
            "Subject: trunk".into(),
            "Organization: Notrealtel".into(),
            "Warning: 399 warn-agent.warnhost.zz \"Agent unreachable\"".into(),
            "Refer-To: <sip:xfer@refer-domain.de?P-Asserted-Identity=%3Csip%3A%2B2865554444%40pct-domain.de%3E>".into(),
            format!("X-B64: <sip:b64@b64-domain.de;ident={b64num}>"),
            "Record-Route: <sip:notrealtel.fi;lr>".into(),
            "X-Corp-Host: sbc2.fakecorp-test.internal".into(),
        ],
        'b' => vec![
            "Via: SIP/2.0/UDP edge.viahost.zz:5060;branch=z9hG4bKbb1".into(),
            "Refer-To: <sip:%2B2861112222@edge.viahost.zz?Replaces=abc%40edge.viahost.zz%3Bto-tag%3D1%3Bfrom-tag%3D2>".into(),
            "Referred-By: <tel:+28-61-21-21-21>".into(),
            "P-Preferred-Identity: <sip:002869990000@edge.viahost.zz>".into(),
            "X-Note: \"peer 3fff:e0a:1:2::9 and [3fff:e0a:1:2::8]:5060\"".into(),
            "Diversion: <sip:+2864445555@edge.viahost.zz>;reason=unconditional".into(),
            "History-Info: <sip:+2862223333@edge.viahost.zz?Reason=SIP%3Bcause%3D302>;index=1".into(),
        ],
        _ => vec![
            "Via: SIP/2.0/UDP sbc.porthost.zz:5070;branch=z9hG4bKcc1".into(),
            "Via: SIP/2.0/UDP 85.12.34.50:5080;branch=z9hG4bKcc2".into(),
            "Warning: 399 sbc.porthost.zz:5070 \"x\", 399 85.12.34.50:5080 \"y\"".into(),
            "Call-Info: <http://sbc.porthost.zz:5070/a>;purpose=info".into(),
        ],
    };
    lines
}

/// The identities each probe carries, none of which may survive.
fn identities(variant: char) -> &'static [&'static str] {
    match variant {
        'a' => &[
            "fakey.mcfakename",
            "85.12.34.201",
            "35000099-176148-1",
            "testo.placeholder",
            "prefid-domain",
            "001019876ABCDE1F",
            "Fakey",
            "McFakename",
            "fakecorp-certs",
            "geo-cid-domain",
            "geo-lis-domain",
            "85.12.34.199",
            "Fakenetone",
            "Fakenettwo",
            "Subject: trunk",
            "Organization: Notrealtel",
            "warnhost",
            "refer-domain",
            "2865554444",
            "pct-domain",
            "b64-domain",
            "notrealtel.fi",
            "fakecorp-test",
            "2862345678",
            "2868765432",
            "KzI4Njc3Nzg4ODg=",
            "c2lnbmF0dXJl",
            "fmcfake",
            "85.12.34.10",
            "Appel",
            "sdp-uri-domain",
            "sdp-email-domain",
            "11 22 33 4",
            "85.12.34.11",
            "testo.dummy",
            "pidf-domain",
            "Fakeville",
            "12.3456",
            "AQAQYAAK",
        ],
        'b' => &[
            "viahost",
            "2861112222",
            "28-61-21-21-21",
            "69990000",
            "3fff:e0a:1:2::9",
            "3fff:e0a:1:2::8",
            "2864445555",
            "2862223333",
        ],
        _ => &["porthost", "85.12.34.50"],
    }
}

/// An INVITE carrying `extra` and a multipart body of an SDP, a PIDF-LO and an
/// ISUP part.
fn probe(extra: &[String]) -> Flows {
    let sdp = "v=0\r\no=fmcfake 1234 5678 IN IP4 85.12.34.10\r\ns=Appel Fakey McFakename\r\ni=Session of Fakey\r\n\
               u=http://www.sdp-uri-domain.de/x\r\ne=f.mcfake@sdp-email-domain.zz\r\np=+28 6 11 22 33 4\r\n\
               c=IN IP4 85.12.34.11\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\n";
    let pidf = "<?xml version=\"1.0\"?><presence xmlns=\"urn:ietf:params:xml:ns:pidf\" entity=\"pres:testo.dummy@pidf-domain.de\">\
                <tuple id=\"t1\"><status><geopriv><location-info><civicAddress><A3>Fakeville</A3><RD>Rue Fakey McFakename</RD>\
                <HNO>12</HNO></civicAddress><gml:pos>12.3456 -1.2345</gml:pos></location-info></geopriv></status></tuple></presence>";
    let isup = base64::engine::general_purpose::STANDARD.encode([
        0x01, 0x00, 0x10, 0x60, 0x00, 0x0a, 0x00, 0x02, 0x09, 0x09, 0x03, 0x90, 0x33, 0x22, 0x11,
        0x44, 0x55, 0x66, 0x77,
    ]);
    let body = format!(
        "--bnd42\r\nContent-Type: application/sdp\r\n\r\n{sdp}--bnd42\r\nContent-Type: application/pidf+xml\r\n\
         Content-ID: <loc1@geo-cid-domain.de>\r\n\r\n{pidf}\r\n--bnd42\r\nContent-Type: application/isup;version=itu-t92+\r\n\
         Content-Transfer-Encoding: base64\r\n\r\n{isup}\r\n--bnd42--\r\n"
    );
    let raw = format!(
        "INVITE sip:+2868765432@as.alpha-carrier.zz SIP/2.0\r\n{}\r\n\
         Via: SIP/2.0/UDP 85.12.34.3:5060;branch=z9hG4bK1\r\nMax-Forwards: 70\r\n\
         From: <sip:+2862345678@alpha-carrier.zz>;tag=f1\r\nTo: <sip:+2868765432@alpha-carrier.zz>\r\n\
         Call-ID: probe1@alpha-carrier.zz\r\nCSeq: 1 INVITE\r\n\
         Content-Type: multipart/mixed;boundary=bnd42\r\nContent-Length: {}\r\n\r\n{body}",
        extra.join("\r\n"),
        body.len()
    );
    let doc = json!({
        "schema": crate::model::SUPPORTED_SCHEMA,
        "decode_stats": { "records": 0, "non_ip": 0, "non_udp": 0, "snap_truncated": 0, "datagrams": 0,
                          "fragments": 0, "reassembled": 0, "frag_dropped": 0, "tail_truncated": 0 },
        "flow_stats": { "sip_messages": 1, "capture_dups": 0, "parse_failed": 0, "non_sip": 0 },
        "legs": [{ "call_id": "probe1@alpha-carrier.zz", "hops": [{ "a": "85.12.34.3:5060", "b": "172.16.245.246:5060" }],
                   "invite": null, "final_status": null, "saw_180": false, "terminated_by": null, "tokens": [],
                   "msgs": [{ "ts_us": 0, "src": "85.12.34.3:5060", "dst": "172.16.245.246:5060", "hop": 0,
                              "retx": false, "raw": raw,
                              "summary": { "kind": "request", "method": "INVITE",
                                           "uri": "sip:+2868765432@as.alpha-carrier.zz",
                                           "cseq": { "seq": 1, "method": "INVITE" },
                                           "from": { "uri": "sip:+2862345678@alpha-carrier.zz", "tag": "f1" },
                                           "to": { "uri": "sip:+2868765432@alpha-carrier.zz", "tag": null } } }] }],
        "groups": []
    });
    ingest_str(&doc.to_string()).expect("the probe ingests")
}

fn wire(flows: &Flows) -> String {
    String::from_utf8_lossy(&crate::model::wire(&flows.legs[0].msgs[0])).into_owned()
}

/// Every leak count of `--verify` over one anonymisation, summed.
fn leaks(
    raw: &Flows,
    out: &Flows,
    plan: &Plan,
    scope: &Scope,
    fakes: &crate::anonymize::Fakes,
) -> usize {
    crate::verify::survivors(raw, out, plan, scope)
        + crate::verify::unmasked_displays(out, scope)
        + crate::verify::names::name_survivors(raw, out, scope)
        + crate::verify::leaks::leaks(out, scope, fakes, &crate::testflow::reading()).total()
}

#[test]
fn every_identity_of_every_probe_is_gone_and_verify_counts_nothing() {
    let (plan, scope) = (crate::testflow::plan(), Scope::default());
    for variant in ['a', 'b', 'c'] {
        let raw = probe(&extra(variant));
        let mut out = probe(&extra(variant));
        let parsed = parse_ok_map(&out, &crate::testflow::reading());
        let report =
            anonymize_in(&mut out, &plan, &Seed::from_bytes([3; 32]), &scope).expect("anonymises");
        let text = wire(&out).to_ascii_lowercase();
        for id in identities(variant) {
            assert!(!text.contains(&id.to_ascii_lowercase()), "probe {variant}: {id} survives");
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
            leaks(&raw, &out, &plan, &scope, &report.fakes),
            0,
            "probe {variant}: verify counts a leak"
        );
        if variant == 'c' {
            assert!(text.contains(":5070") && text.contains(":5080"), "ports are never faked");
        }
        if variant == 'a' {
            assert_eq!(
                report.redacted_parts, 2,
                "the PIDF-LO and the ISUP part are redacted whole"
            );
        }
    }
}

#[test]
fn verify_counts_what_a_probe_leaves_when_nothing_anonymises_it() {
    let (plan, scope) = (crate::testflow::plan(), Scope::default());
    for variant in ['a', 'b', 'c'] {
        let raw = probe(&extra(variant));
        let none = crate::anonymize::Fakes::default();
        assert!(leaks(&raw, &probe(&extra(variant)), &plan, &scope, &none) > 0, "probe {variant}");
    }
}

#[test]
fn an_anonymised_document_is_refused_and_a_raw_one_reproduces_byte_for_byte() {
    let (plan, scope, seed) =
        (crate::testflow::plan(), Scope::default(), Seed::from_bytes([3; 32]));
    let mut once = probe(&extra('a'));
    anonymize_in(&mut once, &plan, &seed, &scope).expect("anonymises");
    assert_eq!(once.rewritten.as_deref(), Some("anonymised"));
    assert!(matches!(
        anonymize_in(&mut once, &plan, &seed, &scope),
        Err(AnonymizeError::AlreadyRewritten(_))
    ));
    let mut twice = probe(&extra('a'));
    anonymize_in(&mut twice, &plan, &seed, &scope).expect("anonymises");
    assert_eq!(wire(&once), wire(&twice));
}
