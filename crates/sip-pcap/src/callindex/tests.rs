use super::*;
use crate::emit::flows_to_doc;
use crate::enrich::EnrichOptions;
use crate::flow::{build_flows, FlowConfig};
use crate::{Datagram, DecodeStats};

fn request(method: &str, call_id: &str, cseq: u32) -> Vec<u8> {
    format!(
        "{method} sip:bob@10.0.0.9 SIP/2.0\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK{cseq}{method}\r\n\
         Max-Forwards: 70\r\nFrom: <sip:alice@10.0.0.1>;tag=f1\r\nTo: <sip:bob@10.0.0.9>\r\n\
         Call-ID: {call_id}\r\nCSeq: {cseq} {method}\r\nSubject: a \"quoted\" {{brace}} [bracket]\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn response(status: u16, call_id: &str, cseq: u32, method: &str) -> Vec<u8> {
    format!(
        "SIP/2.0 {status} X\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK{cseq}{method}\r\n\
         From: <sip:alice@10.0.0.1>;tag=f1\r\nTo: <sip:bob@10.0.0.9>;tag=t1\r\nCall-ID: {call_id}\r\n\
         CSeq: {cseq} {method}\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// Two calls, one of them with a Call-ID that spells JSON punctuation.
const ODD: &str = "x{\"}]y@h";

fn document(pretty: bool) -> String {
    let datagrams: Vec<Datagram> = [
        request("INVITE", ODD, 1),
        response(200, ODD, 1, "INVITE"),
        request("BYE", ODD, 2),
        response(200, ODD, 2, "BYE"),
        request("INVITE", "plain@h", 1),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, payload)| Datagram {
        ts_us: i as u64 * 1000,
        src: "10.0.0.1:5060".parse().unwrap(),
        dst: "10.0.0.9:5060".parse().unwrap(),
        payload,
        probe: 0,
    })
    .collect();
    let flows = build_flows(&datagrams, &FlowConfig::default());
    let doc = flows_to_doc(&flows, &DecodeStats::default(), &EnrichOptions::default()).unwrap();
    if pretty {
        serde_json::to_string_pretty(&doc).unwrap()
    } else {
        serde_json::to_string(&doc).unwrap()
    }
}

#[test]
fn every_leg_is_found_with_its_call_id_and_its_span_reads_back_alone() {
    for pretty in [true, false] {
        let text = document(pretty);
        let spans = scan(text.as_bytes()).expect("scans");
        let ids: Vec<&str> = spans.iter().map(|s| s.call_id.as_str()).collect();
        assert_eq!(ids, [ODD, "plain@h"]);
        for span in &spans {
            let leg: LegJson =
                serde_json::from_str(&text[span.start as usize..span.end as usize]).expect("a leg");
            assert_eq!(leg.call_id, span.call_id);
        }
    }
}

#[test]
fn a_stored_index_serves_until_the_document_changes() {
    let dir = std::env::temp_dir().join(format!("callindex-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let doc = dir.join("capture.flows.json");
    std::fs::write(&doc, document(true)).unwrap();
    let written = write(&doc).expect("indexes");
    assert_eq!(load(&doc).expect("loads"), (written.clone(), true));
    let legs = legs(&doc, &written, ODD).expect("reads");
    assert_eq!(legs.len(), 1);
    assert_eq!(legs[0].msgs.len(), 4);
    std::fs::write(&doc, document(false)).unwrap();
    let (fresh, served) = load(&doc).expect("loads");
    assert!(!served, "a changed document is scanned again");
    assert_eq!(fresh.legs.len(), 2);
    assert!(load(&doc).expect("loads").1, "and its index rewritten beside it");
    let mut old = fresh.clone();
    old.version = INDEX_VERSION + 1;
    std::fs::write(index_path(&doc), serde_json::to_string(&old).unwrap()).unwrap();
    assert!(!load(&doc).expect("loads").1, "an index of another version is stale");
    assert_eq!(load(&doc).expect("loads").0.version, INDEX_VERSION, "and rewritten");
    let _ = std::fs::remove_file(index_path(&doc));
    let _ = std::fs::remove_file(&doc);
}

#[test]
fn a_selection_names_a_transaction_request_and_responses_together() {
    let text = document(true);
    let spans = scan(text.as_bytes()).unwrap();
    let leg: LegJson =
        serde_json::from_str(&text[spans[0].start as usize..spans[0].end as usize]).unwrap();
    let count = |cseq, method| leg.msgs.iter().filter(|m| selects(m, cseq, method)).count();
    assert_eq!(count(Some(2), None), 2);
    assert_eq!(count(None, Some("bye")), 2);
    assert_eq!(count(Some(1), Some("INVITE")), 2);
    assert_eq!(count(None, None), 4);
}

#[test]
fn a_truncated_document_is_refused() {
    let text = document(false);
    assert!(scan(&text.as_bytes()[..text.len() / 2]).is_err());
}
