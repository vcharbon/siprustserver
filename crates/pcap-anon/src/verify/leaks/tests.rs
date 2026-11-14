use super::*;
use crate::model::ingest_str;

/// A one-message document carrying `extra` header lines.
fn doc(extra: &str) -> Flows {
    let raw = format!(
        "OPTIONS sip:h.alpha-carrier.zz SIP/2.0\r\nMax-Forwards: 70\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
         From: <sip:a@10.0.0.1>;tag=1\r\nTo: <sip:b@10.0.0.2>\r\nCall-ID: c@x\r\nCSeq: 1 OPTIONS\r\n\
         {extra}Content-Length: 0\r\n\r\n"
    );
    let json = serde_json::json!({
        "schema": crate::model::SUPPORTED_SCHEMA,
        "decode_stats": { "records": 0, "non_ip": 0, "non_udp": 0, "snap_truncated": 0, "datagrams": 0,
                          "fragments": 0, "reassembled": 0, "frag_dropped": 0, "tail_truncated": 0 },
        "flow_stats": { "sip_messages": 1, "capture_dups": 0, "parse_failed": 0, "non_sip": 0 },
        "legs": [{ "call_id": "c@x", "hops": [{ "a": "10.0.0.1:5060", "b": "10.0.0.2:5060" }], "invite": null, "final_status": null, "saw_180": false,
                   "terminated_by": null, "tokens": [],
                   "msgs": [{ "ts_us": 0, "src": "10.0.0.1:5060", "dst": "10.0.0.2:5060", "hop": 0,
                              "retx": false, "raw": raw,
                              "summary": { "kind": "request", "method": "OPTIONS", "uri": "sip:x",
                                           "cseq": { "seq": 1, "method": "OPTIONS" },
                                           "from": { "uri": "sip:a", "tag": null },
                                           "to": { "uri": "sip:b", "tag": null } } }] }],
        "groups": []
    });
    ingest_str(&json.to_string()).expect("the document ingests")
}

#[test]
fn every_identity_shaped_token_no_fake_explains_is_counted_by_class() {
    let extra = "X-Origin-Host: core.alpha-carrier.zz\r\nX-Peer: 8.8.8.200 and 3fff:e0a:1:2::7\r\n\
                 X-Note: Фейкова\r\nX-Corp: sbc2.fakecorp.internal\r\nSubject: trunk\r\n\
                 Organization: Notrealtel\r\n";
    let leaks =
        leaks(&doc(extra), &Scope::default(), &Fakes::default(), &crate::testflow::reading());
    // The R-URI host, the origin host and the corporate host under any TLD.
    assert_eq!(leaks, Leaks { domains: 3, addresses: 2, words: 2, non_ascii: 1, digits: 0 });
}

#[test]
fn vocabulary_kept_names_and_masks_are_no_leak() {
    let extra = "Contact: <sip:a@anonymous.invalid>;+g.3gpp.ps;+sip.instance\r\n\
                 X-Private: 10.1.2.3\r\nAccept: application/vnd.example.alpha+xml\r\n\
                 P-Asserted-Service: urn:urn-7:3gpp-service.ims.icsi.mmtel\r\nSubject: xxxx\r\n";
    assert_eq!(
        leaks(&doc(extra), &Scope::default(), &Fakes::default(), &crate::testflow::reading()),
        Leaks { domains: 1, ..Leaks::default() },
        "only the R-URI host"
    );
}

#[test]
fn a_digit_run_is_counted_unless_a_counter_names_it_by_its_field() {
    let extra = "X-Called: 15556110006\r\nExpires: 3600\r\nX-Ref: <sip:h.example.com:5070>\r\n\
                 Date: Sat, 13 Nov 2010 23:29:00 GMT\r\nX-Id: abc-4455-0000\r\n";
    let leaks =
        leaks(&doc(extra), &Scope::default(), &Fakes::default(), &crate::testflow::reading());
    assert_eq!(leaks.digits, 1, "15556110006; 4455 is narrower than a phone number: {leaks:?}");
}

#[test]
fn a_name_under_a_sip_first_label_or_after_a_double_slash_is_a_host() {
    let extra = "X-Hop: sip.hidden-xhop.zz\r\nX-Note: \"see http://g.note-hidden.zz/x\"\r\n\
                 Contact: <sip:a@10.0.0.1>;+sip.instance;+g.3gpp.icsi-ref\r\n";
    let leaks =
        leaks(&doc(extra), &Scope::default(), &Fakes::default(), &crate::testflow::reading());
    assert_eq!(leaks.domains, 3, "the R-URI host and both names: {leaks:?}");
}
