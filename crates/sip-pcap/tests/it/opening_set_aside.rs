//! A leg whose opening request extraction set aside is marked in the flows
//! document, and the census judges no dialog or transaction on it: only the
//! rules that read one message alone run there.

use sip_message::ToolReading;
use sip_pcap::doc::FlowsDoc;
use sip_pcap::emit::flows_to_doc;
use sip_pcap::enrich::EnrichOptions;
use sip_pcap::flow::{build_flows, FlowConfig};
use sip_pcap::rfc::scan;
use sip_pcap::{Datagram, DecodeStats};

const ALICE: &str = "192.0.2.1:5060";
const BOB: &str = "192.0.2.2:5060";

fn dg(ts_us: u64, src: &str, dst: &str, payload: String) -> Datagram {
    Datagram {
        ts_us,
        src: src.parse().expect("address"),
        dst: dst.parse().expect("address"),
        payload: payload.into_bytes(),
        probe: 0,
    }
}

/// The common lines of every message of the call: its one Via, the dialog's
/// From and To (`to_tag` once Bob answered) and the CSeq.
fn lines(to_tag: &str, cseq: &str) -> String {
    let to_tag = if to_tag.is_empty() { String::new() } else { format!(";tag={to_tag}") };
    format!(
        "Via: SIP/2.0/UDP {ALICE};branch=z9hG4bK-c1\r\n\
         From: <sip:alice@example.com>;tag=f1\r\n\
         To: <sip:bob@example.com>{to_tag}\r\n\
         Call-ID: c1\r\n\
         CSeq: {cseq}\r\n"
    )
}

fn request(method: &str, to_tag: &str, cseq: &str, contact: &str) -> String {
    format!(
        "{method} sip:bob@example.com SIP/2.0\r\n{}Max-Forwards: 70\r\n{contact}Content-Length: 0\r\n\r\n",
        lines(to_tag, cseq)
    )
}

fn response(status: &str, to_tag: &str, cseq: &str) -> String {
    format!("SIP/2.0 {status}\r\n{}Content-Length: 0\r\n\r\n", lines(to_tag, cseq))
}

/// Alice's INVITE, Bob's 100 and 180, her CANCEL and its 200, Bob's 487 and
/// her ACK: a call cancelled while ringing. `contact` is the INVITE's Contact
/// line; without one the INVITE is refused (RFC 3261 §8.1.1.8).
fn cancelled_call(contact: &str) -> FlowsDoc {
    let datagrams = vec![
        dg(1_000, ALICE, BOB, request("INVITE", "", "1 INVITE", contact)),
        dg(2_000, BOB, ALICE, response("100 Trying", "", "1 INVITE")),
        dg(3_000, BOB, ALICE, response("180 Ringing", "t1", "1 INVITE")),
        dg(4_000, ALICE, BOB, request("CANCEL", "", "1 CANCEL", "")),
        dg(5_000, BOB, ALICE, response("200 OK", "t1", "1 CANCEL")),
        dg(6_000, BOB, ALICE, response("487 Request Terminated", "t1", "1 INVITE")),
        dg(7_000, ALICE, BOB, request("ACK", "t1", "1 ACK", "")),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::standard(ToolReading::plain_rfc()));
    flows_to_doc(&flows, &DecodeStats::default(), &EnrichOptions::under(ToolReading::plain_rfc()))
        .expect("model enriches")
}

fn occasions(doc: &FlowsDoc, rule: &str) -> u64 {
    scan(doc).population.get(rule).map_or(0, |p| p.occasions)
}

#[test]
fn a_leg_whose_invite_was_set_aside_opens_no_dialog_or_transaction_occasion() {
    let doc = cancelled_call("");
    assert_eq!(doc.flow_stats.parse_failed, 1, "the INVITE is refused");
    assert_eq!(doc.legs.len(), 1);
    assert!(doc.legs[0].opening_request_set_aside, "the leg is marked");
    let json = serde_json::to_value(&doc).expect("document serializes");
    assert_eq!(json["legs"][0]["opening_request_set_aside"], true);

    for rule in ["response-cseq-matches-transaction", "mid-dialog-tags"] {
        assert_eq!(occasions(&doc, rule), 0, "{rule} judges a leg with no INVITE");
    }
    assert!(scan(&doc).hits.is_empty(), "{:?}", scan(&doc).hits);
    assert!(occasions(&doc, "to-tag-presence") > 0, "a rule reading one message still runs");
}

#[test]
fn the_same_call_with_its_invite_read_is_judged_whole() {
    let doc = cancelled_call("Contact: <sip:alice@192.0.2.1>\r\n");
    assert_eq!(doc.flow_stats.parse_failed, 0);
    assert!(!doc.legs[0].opening_request_set_aside);
    let json = serde_json::to_value(&doc).expect("document serializes");
    assert!(json["legs"][0].get("opening_request_set_aside").is_none(), "absent when false");

    assert!(occasions(&doc, "response-cseq-matches-transaction") > 0);
    assert!(scan(&doc).hits.is_empty(), "{:?}", scan(&doc).hits);
}
