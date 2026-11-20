//! A name's fake never writes a number key as a whole digit run: a charging
//! id's hashed digits that happened to spell a short number of the capture
//! would leave that number in clear, and the leak check counts it wherever
//! the datagram's width changed. Every value is invented.

use serde_json::json;

use crate::anonymize::anonymize_in;
use crate::fake::address::Scope;
use crate::fake::Seed;
use crate::model::{ingest_str, Flows};

const ICID: &str = "fakeicid-4711-zz";

/// One INVITE carrying the charging id and `extra` header lines.
fn doc(extra: &str) -> Flows {
    let raw = format!(
        "INVITE sip:+2868765432@fake-pbx.notreal-carrier.de SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bKfakey1\r\n\
         From: \"Fakey McFakename\" <sip:+2862345678@fake-sbc.notreal-carrier.de>;tag=fakeyf\r\n\
         To: <sip:+2868765432@fake-pbx.notreal-carrier.de>\r\n\
         Call-ID: fakey-call@fake-sbc.notreal-carrier.de\r\nCSeq: 1 INVITE\r\nMax-Forwards: 70\r\n\
         P-Charging-Vector: icid-value={ICID};icid-generated-at=192.0.2.10\r\n\
         {extra}Content-Length: 0\r\n\r\n"
    );
    let doc = json!({
        "schema": crate::model::SUPPORTED_SCHEMA,
        "decode_stats": { "records": 0, "non_ip": 0, "non_udp": 0, "snap_truncated": 0, "datagrams": 0,
                          "fragments": 0, "reassembled": 0, "frag_dropped": 0, "tail_truncated": 0 },
        "flow_stats": { "sip_messages": 1, "capture_dups": 0, "parse_failed": 0, "non_sip": 0 },
        "legs": [{ "call_id": "fakey-call@fake-sbc.notreal-carrier.de",
                   "hops": [{ "a": "192.0.2.10:5060", "b": "198.51.100.20:5060" }],
                   "invite": null, "final_status": null, "saw_180": false, "terminated_by": null, "tokens": [],
                   "msgs": [{ "ts_us": 0, "src": "192.0.2.10:5060", "dst": "198.51.100.20:5060", "hop": 0,
                              "retx": false, "raw": raw,
                              "summary": { "kind": "request", "method": "INVITE",
                                           "uri": "sip:+2868765432@fake-pbx.notreal-carrier.de",
                                           "cseq": { "seq": 1, "method": "INVITE" },
                                           "from": { "uri": "sip:+2862345678@fake-sbc.notreal-carrier.de",
                                                     "tag": "fakeyf" },
                                           "to": { "uri": "sip:+2868765432@fake-pbx.notreal-carrier.de",
                                                   "tag": null } } }] }],
        "groups": []
    });
    ingest_str(&doc.to_string()).expect("the probe ingests")
}

fn scope() -> Scope {
    Scope::default().all_addresses()
}

/// `extra` anonymised: the raw document, the anonymised one and the verdict total.
fn anonymised(extra: &str) -> (Flows, usize) {
    let raw = doc(extra);
    let mut out = doc(extra);
    let report =
        anonymize_in(&mut out, &crate::testflow::plan(), &Seed::from_bytes([9; 32]), &scope())
            .expect("anonymises");
    let verdict = crate::verify::verdict::judge(
        &raw,
        &out,
        &crate::testflow::plan(),
        &scope(),
        &crate::testflow::reading(),
        &report,
    );
    (out, verdict.total())
}

/// The charging id as the anonymised datagram carries it.
fn icid(flows: &Flows) -> String {
    let raw = String::from_utf8_lossy(&crate::model::wire(&flows.legs[0].msgs[0])).into_owned();
    let at = raw.find("icid-value=").expect("the charging vector") + "icid-value=".len();
    raw[at..at + ICID.len()].to_string()
}

/// The value of the anonymised datagram's `name` header line.
fn header(flows: &Flows, name: &str) -> String {
    let raw = String::from_utf8_lossy(&crate::model::wire(&flows.legs[0].msgs[0])).into_owned();
    let prefix = format!("{name}: ");
    raw.lines().find_map(|l| l.strip_prefix(&prefix)).expect("the header").to_string()
}

/// The whole digit runs of `text`.
fn runs(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_digit()).filter(|r| !r.is_empty()).map(String::from).collect()
}

#[test]
fn a_charging_id_fake_never_writes_a_number_of_the_capture() {
    // The fake of the charging id, alone: its hashed digit group.
    let (alone, _) = anonymised("");
    let group = runs(&icid(&alone)).into_iter().find(|r| r.len() == 4).expect("a 4-digit group");
    assert!(group != "4711" && group.bytes().any(|b| b != b'0'), "the probe needs a fresh group");
    // The same group as a caller's short number: a number key of the capture.
    let extra = format!("P-Asserted-Identity: <sip:{group}@fake-sbc.notreal-carrier.de>\r\n");
    let (out, verdict) = anonymised(&extra);
    let fake = icid(&out);
    assert!(!runs(&fake).contains(&group), "the charging id's fake {fake} writes {group}");
    assert_eq!(verdict, 0, "the leak check counts nothing");
}

#[test]
fn a_host_fake_never_writes_a_number_of_the_capture() {
    let route = "Record-Route: <sip:edge4711.fake-sbc.notreal-carrier.de;lr>\r\n";
    let (alone, _) = anonymised(route);
    let group = runs(&header(&alone, "Record-Route"))
        .into_iter()
        .find(|r| r.len() == 4)
        .expect("the host's 4-digit group");
    assert!(group != "4711" && group.bytes().any(|b| b != b'0'), "the probe needs a fresh group");
    let extra =
        format!("{route}P-Asserted-Identity: <sip:{group}@fake-sbc.notreal-carrier.de>\r\n");
    let (out, verdict) = anonymised(&extra);
    let fake = header(&out, "Record-Route");
    assert!(!runs(&fake).contains(&group), "the host's fake {fake} writes {group}");
    assert_eq!(verdict, 0, "the leak check counts nothing");
}
