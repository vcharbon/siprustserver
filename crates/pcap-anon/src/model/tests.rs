use super::*;

/// [`to_json_string`] under the test reading.
fn emit(flows: &Flows) -> Result<String, String> {
    to_json_string(flows, &crate::testflow::reading())
}

/// A schema-`schema` document whose single message carries `payload`
/// (a JSON fragment for one or more of the three payload forms).
fn minimal_with(schema: &str, payload: &str) -> String {
    format!(
        r#"{{"schema": {schema},
                "decode_stats": {{"records":1,"non_ip":0,"non_udp":0,"snap_truncated":0,
                                  "datagrams":1,"fragments":0,"reassembled":0,"frag_dropped":0,
                                  "tail_truncated":0}},
                "flow_stats": {{"sip_messages":1,"capture_dups":0,"parse_failed":0,"non_sip":0}},
                "legs": [{{"call_id":"c1",
                          "hops":[{{"a":"10.0.0.1:5060","b":"10.0.0.2:5060"}}],
                          "invite":null,"final_status":null,"saw_180":false,
                          "terminated_by":null,"tokens":[],
                          "msgs":[{{"ts_us":7,"src":"10.0.0.1:5060","dst":"10.0.0.2:5060",
                                   "hop":0,"retx":false,{payload},
                                   "summary":{{"kind":"request","method":"OPTIONS","uri":"sip:x@h",
                                              "cseq":{{"seq":1,"method":"OPTIONS"}},
                                              "from":{{"uri":"sip:a@h","tag":"t1"}},
                                              "to":{{"uri":"sip:b@h","tag":null}}}}}}]}}],
                "groups": [{{"legs":[0],"evidence":[]}}]}}"#
    )
}

/// A real OPTIONS datagram — every emit path re-parses the bytes, so a
/// fixture must be a message the parser accepts.
const OPTIONS_RAW: &str = "OPTIONS sip:x@h SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\nMax-Forwards: 70\r\nFrom: <sip:a@h>;tag=t1\r\nTo: <sip:b@h>\r\nCall-ID: c1\r\nCSeq: 1 OPTIONS\r\nContent-Length: 0\r\n\r\n";
/// The same bytes as a JSON string body, backslash escapes intact.
const OPTIONS_JSON: &str = r"OPTIONS sip:x@h SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\nMax-Forwards: 70\r\nFrom: <sip:a@h>;tag=t1\r\nTo: <sip:b@h>\r\nCall-ID: c1\r\nCSeq: 1 OPTIONS\r\nContent-Length: 0\r\n\r\n";
const OPTIONS_B64: &str = "T1BUSU9OUyBzaXA6eEBoIFNJUC8yLjANClZpYTogU0lQLzIuMC9VRFAgMTAuMC4wLjE6NTA2MDticmFuY2g9ejloRzRiSzENCk1heC1Gb3J3YXJkczogNzANCkZyb206IDxzaXA6YUBoPjt0YWc9dDENClRvOiA8c2lwOmJAaD4NCkNhbGwtSUQ6IGMxDQpDU2VxOiAxIE9QVElPTlMNCkNvbnRlbnQtTGVuZ3RoOiAwDQoNCg==";

fn minimal(schema: &str) -> String {
    minimal_with(schema, &format!(r#""raw_b64":"{OPTIONS_B64}""#))
}

#[test]
fn raw_b64_form_parses_and_decodes_raw_bytes() {
    let flows = ingest_str(&minimal("5")).unwrap();
    assert_eq!(wire(&flows.legs[0].msgs[0]), OPTIONS_RAW.as_bytes());
    assert_eq!(flows.legs[0].msgs[0].summary.cseq().method, "OPTIONS");
}

/// The three payload forms all reconstruct the exact wire bytes.
#[test]
fn all_three_payload_forms_reassemble_identically() {
    let text = ingest_str(&minimal_with("5", &format!(r#""raw":"{OPTIONS_JSON}""#))).unwrap();
    assert_eq!(wire(&text.legs[0].msgs[0]), OPTIONS_RAW.as_bytes());
    // `head`+`body_b64`: a UTF-8 head then the two binary body bytes.
    let head = r"OPTIONS sip:x@h SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\nMax-Forwards: 70\r\nFrom: <sip:a@h>;tag=t1\r\nTo: <sip:b@h>\r\nCall-ID: c1\r\nCSeq: 1 OPTIONS\r\nContent-Type: application/octet-stream\r\nContent-Length: 2\r\n\r\n";
    let hb =
        ingest_str(&minimal_with("5", &format!(r#""head":"{head}","body_b64":"AP8=""#))).unwrap();
    let mut want = head.replace("\\r\\n", "\r\n").into_bytes();
    want.extend_from_slice(&[0x00, 0xff]);
    assert_eq!(wire(&hb.legs[0].msgs[0]), want);
}

/// Emit is a pure function of the bytes: a UTF-8 payload re-emits as `raw`,
/// byte-idempotent through ingest.
#[test]
fn emit_form_is_derived_from_bytes_and_round_trips() {
    let flows = ingest_str(&minimal("5")).unwrap();
    let emitted = emit(&flows).unwrap();
    assert!(emitted.contains(r#""raw": "OPTIONS sip:x@h"#), "{emitted}");
    assert!(!emitted.contains("raw_b64"), "{emitted}");
    let again = ingest_str(&emitted).unwrap();
    assert_eq!(emit(&again).unwrap(), emitted);
    assert_eq!(wire(&again.legs[0].msgs[0]), OPTIONS_RAW.as_bytes());
}

/// An OPTIONS with a binary body, read as `raw_b64`, is written as
/// `head`+`body_b64`, byte-idempotent through ingest.
#[test]
fn a_binary_body_is_written_as_head_and_body_b64() {
    let head = "OPTIONS sip:x@h SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
                Max-Forwards: 70\r\nFrom: <sip:a@h>;tag=t1\r\nTo: <sip:b@h>\r\nCall-ID: c1\r\n\
                CSeq: 1 OPTIONS\r\nContent-Type: application/octet-stream\r\nContent-Length: 3\r\n\r\n";
    let mut raw = head.as_bytes().to_vec();
    raw.extend_from_slice(&[0x30, 0x82, 0xff]);
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
    let flows = ingest_str(&minimal_with("5", &format!(r#""raw_b64":"{b64}""#))).unwrap();
    let emitted = emit(&flows).unwrap();
    let msg =
        serde_json::from_str::<serde_json::Value>(&emitted).unwrap()["legs"][0]["msgs"][0].clone();
    assert_eq!(msg["head"], head, "{emitted}");
    assert_eq!(msg["body_b64"], "MIL/", "{emitted}");
    assert!(msg.get("raw").is_none() && msg.get("raw_b64").is_none(), "{emitted}");
    let again = ingest_str(&emitted).unwrap();
    assert_eq!(emit(&again).unwrap(), emitted);
    assert_eq!(wire(&again.legs[0].msgs[0]), raw);
}

/// The probes the extractor rebased survive the rewrite: a document
/// carrying them re-emits them, one carrying none writes no key.
#[test]
fn aligned_probes_are_carried_through_the_rewrite() {
    let aligned =
        r#""aligned_probes":[{"probe":1,"reference":2,"from_us":0,"offset_us":248000,"pairs":12}]"#;
    let doc = minimal("5").replace(r#""non_sip":0}"#, &format!(r#""non_sip":0,{aligned}}}"#));
    let emitted = emit(&ingest_str(&doc).unwrap()).unwrap();
    let stats = serde_json::from_str::<serde_json::Value>(&emitted).unwrap()["flow_stats"].clone();
    assert_eq!(
        stats["aligned_probes"],
        serde_json::json!([{"probe":1,"reference":2,"from_us":0,"offset_us":248000,"pairs":12}]),
        "{emitted}"
    );
    let bare = emit(&ingest_str(&minimal("5")).unwrap()).unwrap();
    assert!(!bare.contains("aligned_probes"), "{bare}");
}

/// The set-aside counts by reason key survive the rewrite, a reader of
/// the anonymised document finding them; one carrying none writes no key.
#[test]
fn set_aside_counts_by_reason_are_carried_through_the_rewrite() {
    let tables = r#""parse_failed_by_reason":{"INVITE has no Contact (RFC 3261 §8.1.1.8)":2},"header_failed":1,"header_failed_by_reason":{"Strict Diversion: unterminated quoted value of parameter n":1}"#;
    let doc = minimal("5").replace(r#""non_sip":0}"#, &format!(r#""non_sip":0,{tables}}}"#));
    let emitted = emit(&ingest_str(&doc).unwrap()).unwrap();
    let stats = serde_json::from_str::<serde_json::Value>(&emitted).unwrap()["flow_stats"].clone();
    assert_eq!(
        stats["parse_failed_by_reason"],
        serde_json::json!({"INVITE has no Contact (RFC 3261 §8.1.1.8)": 2}),
        "{emitted}"
    );
    assert_eq!(
        stats["header_failed_by_reason"],
        serde_json::json!({"Strict Diversion: unterminated quoted value of parameter n": 1}),
        "{emitted}"
    );
    let bare = emit(&ingest_str(&minimal("5")).unwrap()).unwrap();
    assert!(!bare.contains("_by_reason"), "{bare}");
}

/// A leg's opening-request mark survives the rewrite, a reader of the
/// anonymised document finding it; an unmarked leg writes no key.
#[test]
fn the_opening_request_mark_is_carried_through_the_rewrite() {
    let doc = minimal("5")
        .replace(r#""saw_180":false,"#, r#""saw_180":false,"opening_request_set_aside":true,"#);
    let emitted = emit(&ingest_str(&doc).unwrap()).unwrap();
    let leg = serde_json::from_str::<serde_json::Value>(&emitted).unwrap()["legs"][0].clone();
    assert_eq!(leg["opening_request_set_aside"], true, "{emitted}");
    let bare = emit(&ingest_str(&minimal("5")).unwrap()).unwrap();
    assert!(!bare.contains("opening_request_set_aside"), "{bare}");
}

#[test]
fn unknown_schema_is_rejected_loudly() {
    let err = ingest_str(&minimal("9")).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unsupported flows schema 9"), "{msg}");
    assert!(msg.contains("supports schema 5"), "{msg}");
}

#[test]
fn missing_schema_is_rejected() {
    let err = ingest_str(r#"{"legs": []}"#).unwrap_err();
    assert!(err.to_string().contains("no numeric top-level \"schema\""));
}

#[test]
fn several_payload_forms_at_once_is_rejected() {
    let err = ingest_str(&minimal_with(
        "5",
        &format!(r#""raw":"{OPTIONS_JSON}","raw_b64":"{OPTIONS_B64}""#),
    ))
    .unwrap_err();
    assert!(err.to_string().contains("exactly one of"), "{}", err);
}

#[test]
fn out_of_range_hop_index_is_rejected() {
    let broken = minimal("5").replace(r#""hop":0"#, r#""hop":5"#);
    let err = ingest_str(&broken).unwrap_err();
    assert!(err.to_string().contains("hop index 5 out of range"));
}

/// A split form missing its body, and an opaque form that is no base64, are
/// refused at ingest: every message of an ingested document has its bytes.
#[test]
fn a_partial_or_undecodable_payload_is_rejected() {
    let head_alone = ingest_str(&minimal_with("5", &format!(r#""head":"{OPTIONS_JSON}""#)));
    assert!(head_alone.unwrap_err().to_string().contains("exactly one of"));
    let not_base64 = ingest_str(&minimal_with("5", r#""raw_b64":"%%%""#)).unwrap_err();
    assert!(not_base64.to_string().contains("leg 0 msg 0: payload"), "{not_base64}");
}

/// The failed header readings a document carries go out with it: a document
/// taken as is keeps what its extraction found.
#[test]
fn the_failed_header_readings_are_carried_through() {
    let failures = r#""header_failed":1,"header_failures":[{"call_id":"c1","msg":0,"header":"Diversion","reason":"unterminated quoted value"}]"#;
    let doc = minimal("5").replace(r#""non_sip":0}"#, &format!(r#""non_sip":0,{failures}}}"#));
    let emitted = emit(&ingest_str(&doc).unwrap()).unwrap();
    let stats = serde_json::from_str::<serde_json::Value>(&emitted).unwrap()["flow_stats"].clone();
    assert_eq!(stats["header_failures"][0]["header"], "Diversion", "{emitted}");
}
