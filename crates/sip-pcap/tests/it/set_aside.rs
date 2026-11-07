//! The messages extraction sets aside are counted by refusal reason key: one
//! row per shape, whatever values each message carries, for the messages the
//! parser refuses and for the failed header readings of the ones it reads, and
//! the flows document carries both tables.

use sip_message::ToolReading;
use sip_pcap::emit::flows_to_doc;
use sip_pcap::enrich::EnrichOptions;
use sip_pcap::flow::{build_flows, FlowConfig};
use sip_pcap::{Datagram, DecodeStats};

fn dg(ts_us: u64, payload: String) -> Datagram {
    Datagram {
        ts_us,
        src: "192.0.2.1:5060".parse().expect("address"),
        dst: "192.0.2.2:5060".parse().expect("address"),
        payload: payload.into_bytes(),
        probe: 0,
    }
}

/// An INVITE on `call_id` from `from`, `contact` and `extra` lines included.
fn invite(call_id: &str, from: &str, contact: &str, extra: &str) -> String {
    format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
         Via: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-{call_id}\r\n\
         Max-Forwards: 70\r\n\
         From: {from};tag=f1\r\n\
         To: <sip:bob@example.com>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 INVITE\r\n\
         {contact}{extra}Content-Length: 0\r\n\r\n"
    )
}

const CONTACT: &str = "Contact: <sip:alice@192.0.2.1>\r\n";

#[test]
fn two_refusal_shapes_give_two_rows_whatever_values_each_message_carries() {
    let datagrams = vec![
        // No Contact, twice.
        dg(1_000, invite("c1", "<sip:alice@example.com>", "", "")),
        dg(2_000, invite("c2", "<sip:carol@example.net>", "", "")),
        // A From URI parameter repeated with another value, twice, each with
        // its own parameter and values.
        dg(3_000, invite("c3", "<sip:alice@example.com;x-a=1;x-a=2>", CONTACT, "")),
        dg(4_000, invite("c4", "<sip:dave@example.org;x-b=3;x-b=4>", CONTACT, "")),
        // Read, with a Diversion whose quoted parameter value never closes,
        // twice, each with its own value.
        dg(
            5_000,
            invite(
                "c5",
                "<sip:alice@example.com>",
                CONTACT,
                "Diversion: <sip:e@example.com>;n=\"x\r\n",
            ),
        ),
        dg(
            6_000,
            invite(
                "c6",
                "<sip:alice@example.com>",
                CONTACT,
                "Diversion: <sip:f@example.org>;n=\"yy\r\n",
            ),
        ),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::standard(ToolReading::plain_rfc()));
    assert_eq!((flows.stats.parse_failed, flows.stats.header_failed), (4, 2));

    let refused: Vec<(&str, u64)> =
        flows.stats.parse_failed_by_reason.iter().map(|(k, &n)| (k.as_str(), n)).collect();
    assert_eq!(refused.len(), 2, "one row per shape: {refused:?}");
    assert!(refused.contains(&("INVITE has no Contact (RFC 3261 §8.1.1.8)", 2)), "{refused:?}");
    let repeat = refused.iter().find(|(k, _)| k.contains("From")).expect("the repeat's row");
    assert_eq!(repeat.1, 2, "{refused:?}");
    assert!(!repeat.0.contains("x-a") && !repeat.0.contains("x-b"), "{refused:?}");

    let header: Vec<(&str, u64)> =
        flows.stats.header_failed_by_reason.iter().map(|(k, &n)| (k.as_str(), n)).collect();
    assert_eq!(header.len(), 1, "one row for the two Diversions: {header:?}");
    assert_eq!(header[0].1, 2, "{header:?}");
    assert!(header[0].0.starts_with("Strict Diversion"), "{header:?}");

    let doc = flows_to_doc(
        &flows,
        &DecodeStats::default(),
        &EnrichOptions::under(ToolReading::plain_rfc()),
    )
    .expect("model enriches");
    let json = serde_json::to_value(&doc).expect("document serializes");
    let stats = &json["flow_stats"];
    assert_eq!(stats["parse_failed_by_reason"]["INVITE has no Contact (RFC 3261 §8.1.1.8)"], 2);
    assert_eq!(stats["parse_failed_by_reason"].as_object().expect("a table").len(), 2);
    assert_eq!(stats["header_failed_by_reason"][header[0].0], 2);
}

#[test]
fn a_capture_with_nothing_set_aside_states_no_table() {
    let datagrams = vec![dg(1_000, invite("c1", "<sip:alice@example.com>", CONTACT, ""))];
    let flows = build_flows(&datagrams, &FlowConfig::standard(ToolReading::plain_rfc()));
    let doc = flows_to_doc(
        &flows,
        &DecodeStats::default(),
        &EnrichOptions::under(ToolReading::plain_rfc()),
    )
    .expect("model enriches");
    let json = serde_json::to_value(&doc).expect("document serializes");
    let stats = json["flow_stats"].as_object().expect("stats");
    assert!(!stats.contains_key("parse_failed_by_reason"), "{stats:?}");
    assert!(!stats.contains_key("header_failed_by_reason"), "{stats:?}");
}

/// Every key of `datagrams`' set-aside tables, refused and header-failed.
fn keys_of(datagrams: &[Datagram]) -> Vec<String> {
    let flows = build_flows(datagrams, &FlowConfig::standard(ToolReading::plain_rfc()));
    let stats = &flows.stats;
    stats
        .parse_failed_by_reason
        .keys()
        .chain(stats.header_failed_by_reason.keys())
        .cloned()
        .collect()
}

/// A key holds none of the values its message carried: a Contact's display
/// name, one holding an escaped quote, a Request-URI, a parameter's name.
#[test]
fn a_reason_key_holds_no_value_of_its_message() {
    let datagrams = vec![
        dg(
            1_000,
            invite(
                "c1",
                "<sip:alice@example.com>",
                "Contact: \"Fakey McFakename\" <sip:secretuser@203.0.113.5;x-a=1;x-a=2>\r\n",
                "",
            ),
        ),
        dg(
            2_000,
            invite(
                "c2",
                "<sip:alice@example.com>",
                "Contact: \"Fakey \\\"Q\\\" McFakename\" <sip:secretuser@203.0.113.5;x-b=1;x-b=2>\r\n",
                "",
            ),
        ),
        dg(
            3_000,
            invite("c3", "<sip:alice@example.com>", CONTACT, "").replacen(
                "INVITE sip:bob@example.com SIP/2.0",
                "INVITE sip:secretuser@example.com?Subject=McFakename SIP/2.0",
                1,
            ),
        ),
        dg(
            4_000,
            invite(
                "c4",
                "<sip:alice@example.com>",
                CONTACT,
                "Diversion: <sip:e@example.com>;secretparam=\"x\r\n",
            ),
        ),
    ];
    let keys = keys_of(&datagrams);
    assert_eq!(
        keys.len(),
        3,
        "the two Contacts on one row, the Request-URI, the parameter: {keys:?}"
    );
    for key in &keys {
        for value in ["Fakey", "McFakename", "secret", "203", "113", "x-a", "x-b", "Subject"] {
            assert!(!key.contains(value), "{value:?} in {key:?}");
        }
    }
}

/// A header refused by a value gate is named by its canonical name, however
/// its line spells it, so every spelling counts on one row.
#[test]
fn every_spelling_of_a_header_counts_on_one_row() {
    let spellings = ["Content-Length", "content-length", "l"];
    let datagrams: Vec<Datagram> = spellings
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let call_id = format!("s{i}");
            let message = invite(&call_id, "<sip:alice@example.com>", CONTACT, "")
                .replace("Content-Length: 0", &format!("{name}: 0x"));
            dg(1_000 * (i as u64 + 1), message)
        })
        .collect();
    let flows = build_flows(&datagrams, &FlowConfig::standard(ToolReading::plain_rfc()));
    let rows: Vec<(&str, u64)> =
        flows.stats.parse_failed_by_reason.iter().map(|(k, &n)| (k.as_str(), n)).collect();
    assert_eq!(rows, [("Invalid Content-Length numeric value: \"…\"", 3)]);
}
