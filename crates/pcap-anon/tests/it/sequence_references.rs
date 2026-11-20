//! A number that names another message's sequence number comes out exactly
//! as that message's own does, whatever key its digits spell: a PRACK's RAck
//! (RFC 3262 §7.2: the RSeq of the provisional and the CSeq number and
//! method of the INVITE), a REFER's NOTIFY `Event: refer;id=` (RFC 3515
//! §2.4.6: the REFER's CSeq number) and the CSeq of the sipfrag it carries.
//! Every value is invented; each sequence number is also a caller's
//! extension, a key of the number pass.

use serde_json::{json, Value};

use pcap_anon::anonymize::anonymize_in;
use pcap_anon::fake::address::Scope;
use pcap_anon::fake::Seed;
use pcap_anon::model::{ingest_str, Flows, SUPPORTED_SCHEMA};

const CALLER: &str = "<sip:+2861234567@fake-sbc.carrier.invalid>";
const CALLEE: &str = "<sip:+2869876543@fake-pbx.carrier.invalid>";
const CALL_ID: &str = "fakey-41001@fake-sbc.carrier.invalid";

/// One message of the leg: its bytes and the summary the extractor writes.
fn msg(raw: String, summary: Value) -> Value {
    json!({ "ts_us": 0, "src": "192.0.2.10:5060", "dst": "198.51.100.20:5060", "hop": 0,
            "retx": false, "raw": raw, "summary": summary })
}

fn request(method: &str, seq: u32, extra: &str, body: &str) -> Value {
    let raw = format!(
        "{method} sip:+2869876543@fake-pbx.carrier.invalid SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bKfake{seq}\r\n\
         From: \"Fakey McFakename\" {CALLER};tag=fakeyf\r\nTo: {CALLEE};tag=fakeyt\r\n\
         Call-ID: {CALL_ID}\r\nCSeq: {seq} {method}\r\nMax-Forwards: 70\r\n\
         P-Asserted-Identity: <sip:41001@fake-sbc.carrier.invalid>, <sip:52002@fake-sbc.carrier.invalid>\r\n\
         {extra}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let summary = json!({ "kind": "request", "method": method,
                          "uri": "sip:+2869876543@fake-pbx.carrier.invalid",
                          "cseq": { "seq": seq, "method": method },
                          "from": { "uri": "sip:+2861234567@fake-sbc.carrier.invalid", "tag": "fakeyf" },
                          "to": { "uri": "sip:+2869876543@fake-pbx.carrier.invalid", "tag": "fakeyt" } });
    msg(raw, summary)
}

fn response(status: u16, reason: &str, seq: u32, method: &str, extra: &str) -> Value {
    let raw = format!(
        "SIP/2.0 {status} {reason}\r\n\
         Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bKfake{seq}\r\n\
         From: \"Fakey McFakename\" {CALLER};tag=fakeyf\r\nTo: {CALLEE};tag=fakeyt\r\n\
         Call-ID: {CALL_ID}\r\nCSeq: {seq} {method}\r\n{extra}Content-Length: 0\r\n\r\n"
    );
    let summary = json!({ "kind": "response", "status": status, "reason": reason,
                          "cseq": { "seq": seq, "method": method },
                          "from": { "uri": "sip:+2861234567@fake-sbc.carrier.invalid", "tag": "fakeyf" },
                          "to": { "uri": "sip:+2869876543@fake-pbx.carrier.invalid", "tag": "fakeyt" } });
    msg(raw, summary)
}

/// One leg carrying `msgs`, anonymised.
fn anonymised(msgs: Vec<Value>) -> Flows {
    let doc = json!({
        "schema": SUPPORTED_SCHEMA,
        "decode_stats": { "records": 0, "non_ip": 0, "non_udp": 0, "snap_truncated": 0, "datagrams": 0,
                          "fragments": 0, "reassembled": 0, "frag_dropped": 0, "tail_truncated": 0 },
        "flow_stats": { "sip_messages": msgs.len(), "capture_dups": 0, "parse_failed": 0, "non_sip": 0 },
        "legs": [{ "call_id": CALL_ID, "hops": [{ "a": "192.0.2.10:5060", "b": "198.51.100.20:5060" }],
                   "invite": null, "final_status": null, "saw_180": false, "terminated_by": null,
                   "tokens": [], "msgs": msgs }],
        "groups": []
    });
    let mut flows = ingest_str(&doc.to_string()).expect("the probe ingests");
    anonymize_in(
        &mut flows,
        &crate::support::plan(),
        &Seed::from_bytes([7; 32]),
        &Scope::default(),
    )
    .expect("anonymises");
    flows
}

/// The value of the first `name` header line of message `i`.
fn header(flows: &Flows, i: usize, name: &str) -> String {
    let raw = String::from_utf8_lossy(&pcap_anon::model::wire(&flows.legs[0].msgs[i])).into_owned();
    let prefix = format!("{name}: ");
    raw.lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("message {i} carries {name}"))
        .to_string()
}

/// 41001 and 52002 are keys the number pass rewrites where they name no
/// message, so the probes below mean something.
#[test]
fn the_sequence_numbers_stand_as_keys() {
    let out = anonymised(vec![request("OPTIONS", 1, "", "")]);
    let pai = header(&out, 0, "P-Asserted-Identity");
    assert!(!pai.contains("41001") && !pai.contains("52002"), "{pai}");
}

#[test]
fn a_rack_names_the_provisional_and_the_invite_as_they_come_out() {
    let out = anonymised(vec![
        request("INVITE", 41001, "Supported: 100rel\r\n", ""),
        response(183, "Session Progress", 41001, "INVITE", "Require: 100rel\r\nRSeq: 52002\r\n"),
        request("PRACK", 41002, "RAck: 52002 41001 INVITE\r\n", ""),
        response(200, "OK", 41002, "PRACK", ""),
    ]);
    let invite = header(&out, 0, "CSeq");
    let rseq = header(&out, 1, "RSeq");
    let invite_seq = invite.split(' ').next().unwrap();
    assert_eq!(header(&out, 2, "RAck"), format!("{rseq} {invite_seq} INVITE"));
}

#[test]
fn a_refer_notify_names_the_refer_and_its_sipfrag_the_invite_as_they_come_out() {
    let sipfrag = "SIP/2.0 200 OK\r\nCSeq: 41001 INVITE\r\n";
    let out = anonymised(vec![
        request("INVITE", 41001, "", ""),
        response(200, "OK", 41001, "INVITE", ""),
        request("REFER", 52002, "Refer-To: <sip:+2861234567@fake-sbc.carrier.invalid>\r\n", ""),
        response(202, "Accepted", 52002, "REFER", ""),
        request(
            "NOTIFY",
            52003,
            "Event: refer;id=52002\r\nSubscription-State: terminated;reason=noresource\r\n\
             Content-Type: message/sipfrag;version=2.0\r\n",
            sipfrag,
        ),
    ]);
    let invite_seq = header(&out, 0, "CSeq").split(' ').next().unwrap().to_string();
    let refer_seq = header(&out, 2, "CSeq").split(' ').next().unwrap().to_string();
    assert_eq!(header(&out, 4, "Event"), format!("refer;id={refer_seq}"));
    let notify =
        String::from_utf8_lossy(&pcap_anon::model::wire(&out.legs[0].msgs[4])).into_owned();
    let body = notify.split("\r\n\r\n").nth(1).expect("the NOTIFY carries its sipfrag");
    assert_eq!(body, format!("SIP/2.0 200 OK\r\nCSeq: {invite_seq} INVITE\r\n"));
}
