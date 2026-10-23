use super::*;
use sip_pcap::emit::flows_to_doc;
use sip_pcap::enrich::EnrichOptions;
use sip_pcap::flow::{build_flows, FlowConfig};
use sip_pcap::{Datagram, DecodeStats};

fn message(head: &str, call_id: &str, cseq: &str) -> Vec<u8> {
    format!(
        "{head}\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK{}\r\nMax-Forwards: 70\r\n\
         From: <sip:alice@10.0.0.1>;tag=f1\r\nTo: <sip:bob@10.0.0.9>;tag=t1\r\nCall-ID: {call_id}\r\nCSeq: {cseq}\r\nContent-Length: 0\r\n\r\n",
        cseq.replace(' ', "")
    )
    .into_bytes()
}

const ALICE: &str = "10.0.0.1:5060";
const BOB: &str = "10.0.0.9:5060";

/// A directory holding one flows document of two calls.
fn capture(name: &str) -> (PathBuf, PathBuf) {
    let payloads = [
        message("INVITE sip:bob@10.0.0.9 SIP/2.0", "one@h", "1 INVITE"),
        message("SIP/2.0 180 Ringing", "one@h", "1 INVITE"),
        message("SIP/2.0 200 OK", "one@h", "1 INVITE"),
        message("BYE sip:bob@10.0.0.9 SIP/2.0", "one@h", "2 BYE"),
        message("INVITE sip:bob@10.0.0.9 SIP/2.0", "two@h", "1 INVITE"),
    ];
    let datagrams: Vec<Datagram> = payloads
        .into_iter()
        .enumerate()
        .map(|(i, payload)| {
            let (src, dst) =
                if payload.starts_with(b"SIP/2.0") { (BOB, ALICE) } else { (ALICE, BOB) };
            Datagram {
                ts_us: 1_000_000 + i as u64 * 250_000,
                src: src.parse().unwrap(),
                dst: dst.parse().unwrap(),
                payload,
                probe: 0,
            }
        })
        .collect();
    let flows = build_flows(&datagrams, &FlowConfig::default());
    let doc = flows_to_doc(&flows, &DecodeStats::default(), &EnrichOptions::default()).unwrap();
    let dir = std::env::temp_dir().join(format!("sipflow-get-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("capture.flows.json");
    std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    (dir, path)
}

fn args(doc: &Path, extra: &[&str]) -> GetArgs {
    let mut argv = vec!["sipflow get", doc.to_str().unwrap()];
    argv.extend_from_slice(extra);
    GetArgs::parse_from(argv)
}

#[test]
fn a_call_is_served_from_its_index_one_line_per_message() {
    let (dir, doc) = capture("lines");
    sip_pcap::callindex::write(&doc).unwrap();
    let text = run(&args(&dir, &["--call-id", "one@h"])).expect("served");
    assert_eq!(
        text,
        "leg one@h (4 message(s))\n\
         m0 +0.000000 10.0.0.1:5060 -> 10.0.0.9:5060 | INVITE sip:bob@10.0.0.9 | CSeq 1 INVITE\n\
         m1 +0.250000 10.0.0.9:5060 -> 10.0.0.1:5060 | 180 Ringing | CSeq 1 INVITE\n\
         m2 +0.500000 10.0.0.9:5060 -> 10.0.0.1:5060 | 200 OK | CSeq 1 INVITE\n\
         m3 +0.750000 10.0.0.1:5060 -> 10.0.0.9:5060 | BYE sip:bob@10.0.0.9 | CSeq 2 BYE\n"
    );
}

#[test]
fn a_transaction_selects_its_request_and_responses_and_raw_prints_the_wire() {
    let (_, doc) = capture("raw");
    let text =
        run(&args(&doc, &["--call-id", "one@h", "--cseq", "1", "--method", "invite", "--raw"]))
            .expect("served without an index");
    assert_eq!(text.matches("\nm").count(), 3, "{text}");
    assert!(text.contains("SIP/2.0 180 Ringing\r\nVia:"), "{text}");
    assert!(!text.contains("BYE"), "{text}");
}

#[test]
fn an_unknown_call_or_an_ambiguous_directory_is_refused() {
    let (dir, doc) = capture("refused");
    assert!(run(&args(&doc, &["--call-id", "three@h"])).unwrap_err().contains("2 legs indexed"));
    std::fs::write(dir.join("other.flows.json"), "{}").unwrap();
    assert!(document(&dir).unwrap_err().contains("name one"));
}
