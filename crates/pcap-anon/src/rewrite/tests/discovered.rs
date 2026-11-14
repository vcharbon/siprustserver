//! An equivalence corpus whose substitution sets are the ones discovery
//! makes: a document of whole calls — requests of several methods and their
//! responses — dialling numbers in every form the test plan reads (national
//! behind its prefix, international behind `+` and `00`, private, short,
//! unrecognised runs that open on a `0`), anonymised under the golden seed;
//! the run's mapping entries are the digit pairs, its raw display names the
//! display pairs.

use serde_json::json;

use super::{digit_runs, plan_anchors, prioritized_names, reference_mask, Corpus, Sample};
use crate::fake::address::Scope;
use crate::fake::{Seed, GOLDEN_SEED_HEX};
use crate::rewrite::prioritized;

/// The test plan's default country code, its national prefix and the width
/// of its national significant numbers.
const CC: &str = "28";
const NATIONAL: &str = "0";
const NSN_WIDTH: usize = 8;

/// The `n`th national significant number of the corpus.
fn nsn(n: u64) -> String {
    format!("6{:0>w$}", n * 7_919 % 10u64.pow(NSN_WIDTH as u32 - 1), w = NSN_WIDTH - 1)
}

/// The dial forms of call `i`: caller, callee, a diverting party, a
/// private extension, a short code and an unrecognised run.
fn forms(i: u64) -> [String; 6] {
    [
        format!("{NATIONAL}{}", nsn(i)),
        format!("+{CC}{}", nsn(i + 1)),
        format!("00{CC}{}", nsn(i + 2)),
        format!("0{:04}", i % 10_000),
        format!("3{:03}", i % 1_000),
        format!("0{:014}", i * 104_729),
    ]
}

/// One message of the corpus as the flows document states it.
fn message(ts: u64, raw: &str, summary: serde_json::Value) -> serde_json::Value {
    json!({ "ts_us": ts, "src": "10.0.0.2:5060", "dst": "10.0.0.1:5060", "hop": 0,
            "retx": false, "raw": raw, "summary": summary })
}

/// The messages of call `i`: an INVITE answered 180 then 200, a MESSAGE
/// answered 202, a BYE answered 200.
fn call(i: u64) -> serde_json::Value {
    let [caller, callee, diverter, private, short, run] = forms(i);
    let display = ["Foo Testy", "Bar", "AB", "Zed 99"][(i % 4) as usize];
    let call_id = format!("{}-{i}@h", &callee[1..]);
    let (from, to) = (format!("sip:{caller}@h"), format!("sip:{callee}@h"));
    let head = |start: &str, seq: u64, method: &str, to_tag: &str| {
        format!(
            "{start}\r\nVia: SIP/2.0/UDP 10.0.0.2:5060;branch=z9hG4bK{i}{method}\r\n\
             From: \"{display} {private}\" <{from}>;tag=f{i}\r\nTo: <{to}>{to_tag}\r\n\
             Call-ID: {call_id}\r\nCSeq: {seq} {method}\r\n"
        )
    };
    let request = |seq: u64, method: &str, extra: &str, body: &str| {
        let start = format!("{method} sip:{callee}@h SIP/2.0");
        let raw = format!(
            "{}Max-Forwards: 70\r\nContact: <sip:{private}@10.0.0.2:5060>\r\n{extra}\
             Content-Length: {}\r\n\r\n{body}",
            head(&start, seq, method, if seq == 1 { "" } else { ";tag=t1" }),
            body.len()
        );
        let summary = json!({ "kind": "request", "method": method, "uri": format!("sip:{callee}@h"),
            "cseq": { "seq": seq, "method": method },
            "from": { "uri": from, "tag": format!("f{i}") }, "to": { "uri": to, "tag": null } });
        (raw, summary)
    };
    let response = |status: u16, reason: &str, seq: u64, method: &str| {
        let raw = format!(
            "{}Contact: <sip:{short}@10.0.0.1:5060>\r\nContent-Length: 0\r\n\r\n",
            head(&format!("SIP/2.0 {status} {reason}"), seq, method, ";tag=t1")
        );
        let summary = json!({ "kind": "response", "status": status, "reason": reason,
            "cseq": { "seq": seq, "method": method },
            "from": { "uri": from, "tag": format!("f{i}") }, "to": { "uri": to, "tag": "t1" } });
        (raw, summary)
    };
    let sdp =
        format!("v=0\r\no={private} {i}{run} 1 IN IP4 10.0.0.2\r\ns=-\r\nc=IN IP4 10.0.0.2\r\n");
    let diversion = format!(
        "Diversion: <sip:{diverter}@h>;reason=unconditional\r\nP-Asserted-Identity: <sip:{run}@h>\r\n"
    );
    let text = format!("call {caller} back at {run}");
    let msgs = [
        request(1, "INVITE", &format!("{diversion}Content-Type: application/sdp\r\n"), &sdp),
        response(180, "Ringing", 1, "INVITE"),
        response(200, "OK", 1, "INVITE"),
        request(2, "MESSAGE", "Content-Type: text/plain\r\n", &text),
        response(202, "Accepted", 2, "MESSAGE"),
        request(3, "BYE", "", ""),
        response(200, "OK", 3, "BYE"),
    ];
    let msgs: Vec<serde_json::Value> = msgs
        .into_iter()
        .enumerate()
        .map(|(n, (raw, summary))| message(i * 1_000_000 + n as u64, &raw, summary))
        .collect();
    json!({ "call_id": call_id, "hops": [{ "a": "10.0.0.2:5060", "b": "10.0.0.1:5060" }],
            "invite": null, "final_status": null, "saw_180": false, "terminated_by": null,
            "tokens": [], "msgs": msgs })
}

/// The corpus: 200 calls, the pairs discovery makes over them.
pub(super) fn corpus() -> Corpus {
    let doc = json!({
        "schema": crate::model::SUPPORTED_SCHEMA,
        "decode_stats": { "records": 0, "non_ip": 0, "non_udp": 0, "snap_truncated": 0, "datagrams": 0,
                          "fragments": 0, "reassembled": 0, "frag_dropped": 0, "tail_truncated": 0 },
        "flow_stats": { "sip_messages": 0, "capture_dups": 0, "parse_failed": 0, "non_sip": 0 },
        "legs": (0..200).map(call).collect::<Vec<_>>(),
        "groups": []
    });
    let flows = crate::model::ingest_str(&doc.to_string()).expect("the corpus ingests");
    let mut mapped = flows.clone();
    let seed = Seed::from_hex(GOLDEN_SEED_HEX).expect("the golden seed");
    let report = crate::anonymize::anonymize_in(
        &mut mapped,
        &crate::testflow::plan(),
        &seed,
        &Scope::default(),
    )
    .expect("the corpus anonymises");
    let pairs_of = |unrecognised: bool| {
        prioritized(
            report
                .entries
                .iter()
                .filter(|e| (e.class == "unrecognised") == unrecognised)
                .map(|e| (e.original.clone().into_bytes(), e.fake.clone().into_bytes()))
                .collect(),
        )
    };
    let (digit_pairs, weak_pairs) = (pairs_of(false), pairs_of(true));
    let mut displays: Vec<String> = Vec::new();
    let mut samples = Vec::new();
    for msg in flows.legs.iter().flat_map(|leg| &leg.msgs) {
        let raw = crate::model::wire(msg);
        let harvested = crate::identities::harvest(&raw, &Default::default());
        for dn in harvested.displays.iter().map(|d| String::from_utf8_lossy(d).into_owned()) {
            if !displays.contains(&dn) {
                displays.push(dn);
            }
        }
        let cseq = msg.summary.cseq();
        samples.push(Sample {
            raw,
            keep: Some((cseq.seq.to_string().into_bytes(), cseq.method.clone().into_bytes())),
            runs: Vec::new(),
        });
    }
    let display_pairs = prioritized_names(
        displays
            .iter()
            .filter_map(|dn| {
                let masked = reference_mask(dn, &digit_pairs, &weak_pairs, &plan_anchors());
                (masked != *dn).then(|| (dn.clone(), masked))
            })
            .collect(),
    );
    samples.push(Sample {
        raw: Vec::new(),
        keep: None,
        runs: displays.iter().flat_map(|dn| digit_runs(dn)).collect(),
    });
    Corpus { samples, digit_pairs, weak_pairs, anchors: plan_anchors(), display_pairs }
}
