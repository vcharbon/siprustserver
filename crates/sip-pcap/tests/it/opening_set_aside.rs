//! A leg whose opening request extraction set aside, with no copy of it read
//! on any hop, is marked in the flows document, and the census judges no
//! dialog or transaction on it: only the rules that read one message alone run
//! there. Any other refusal leaves the leg as it is.

use sip_message::ToolReading;
use sip_pcap::doc::FlowsDoc;
use sip_pcap::emit::flows_to_doc;
use sip_pcap::enrich::EnrichOptions;
use sip_pcap::flow::{build_flows, FlowConfig};
use sip_pcap::rfc::scan;
use sip_pcap::{Datagram, DecodeStats};

const ALICE: &str = "192.0.2.1:5060";
const PROXY: &str = "192.0.2.3:5060";
const BOB: &str = "192.0.2.2:5060";

/// The INVITE's Contact; an INVITE without one is refused (RFC 3261 §8.1.1.8).
const CONTACT: &str = "Contact: <sip:alice@192.0.2.1>\r\n";
/// Bob's Contact, which his 2xx to an INVITE carries.
const BOB_CONTACT: &str = "Contact: <sip:bob@192.0.2.2>\r\n";
/// A single-valued header stated twice: any message carrying it is refused.
const TWICE: &str = "Subject: one\r\nSubject: two\r\n";

fn dg(ts_us: u64, src: &str, dst: &str, payload: String) -> Datagram {
    Datagram {
        ts_us,
        src: src.parse().expect("address"),
        dst: dst.parse().expect("address"),
        payload: payload.into_bytes(),
        probe: 0,
    }
}

/// One message of call `call_id`: its start line, its Via stack (Alice's,
/// under the proxy's where `proxied`), From, To (`to_tag` once Bob answered),
/// the CSeq, and `extra` lines.
fn message(
    start: &str,
    call_id: &str,
    proxied: bool,
    to_tag: &str,
    cseq: &str,
    extra: &str,
) -> String {
    let to_tag = if to_tag.is_empty() { String::new() } else { format!(";tag={to_tag}") };
    let proxy = if proxied {
        format!("Via: SIP/2.0/UDP {PROXY};branch=z9hG4bK-p-{call_id}\r\n")
    } else {
        String::new()
    };
    let max_forwards = if start.starts_with("SIP/") { "" } else { "Max-Forwards: 70\r\n" };
    format!(
        "{start}\r\n{proxy}Via: SIP/2.0/UDP {ALICE};branch=z9hG4bK-{call_id}\r\n\
         From: <sip:alice@example.com>;tag=f1\r\n\
         To: <sip:bob@example.com>{to_tag}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq}\r\n{max_forwards}{extra}Content-Length: 0\r\n\r\n"
    )
}

fn request(method: &str, call_id: &str, to_tag: &str, cseq: &str, extra: &str) -> String {
    message(&format!("{method} sip:bob@example.com SIP/2.0"), call_id, false, to_tag, cseq, extra)
}

fn response(status: &str, call_id: &str, to_tag: &str, cseq: &str) -> String {
    message(&format!("SIP/2.0 {status}"), call_id, false, to_tag, cseq, "")
}

fn doc(datagrams: &[Datagram]) -> FlowsDoc {
    let flows = build_flows(datagrams, &FlowConfig::standard(ToolReading::plain_rfc()));
    flows_to_doc(&flows, &DecodeStats::default(), &EnrichOptions::under(ToolReading::plain_rfc()))
        .expect("model enriches")
}

/// Alice's INVITE, Bob's 100 and 180, her CANCEL and its 200, Bob's 487 and
/// her ACK: a call cancelled while ringing, the INVITE carrying `invite_extra`.
fn cancelled_call(invite_extra: &str) -> FlowsDoc {
    doc(&[
        dg(1_000, ALICE, BOB, request("INVITE", "c1", "", "1 INVITE", invite_extra)),
        dg(2_000, BOB, ALICE, response("100 Trying", "c1", "", "1 INVITE")),
        dg(3_000, BOB, ALICE, response("180 Ringing", "c1", "t1", "1 INVITE")),
        dg(4_000, ALICE, BOB, request("CANCEL", "c1", "", "1 CANCEL", "")),
        dg(5_000, BOB, ALICE, response("200 OK", "c1", "t1", "1 CANCEL")),
        dg(6_000, BOB, ALICE, response("487 Request Terminated", "c1", "t1", "1 INVITE")),
        dg(7_000, ALICE, BOB, request("ACK", "c1", "t1", "1 ACK", "")),
    ])
}

/// An answered call whose INVITE was read, then `then` from Alice to Bob, the
/// datagram the case refuses, and Alice's BYE and Bob's 200.
fn answered_call_then(then: String) -> FlowsDoc {
    doc(&[
        dg(1_000, ALICE, BOB, request("INVITE", "c1", "", "1 INVITE", CONTACT)),
        dg(
            2_000,
            BOB,
            ALICE,
            message("SIP/2.0 200 OK", "c1", false, "t1", "1 INVITE", BOB_CONTACT),
        ),
        dg(3_000, ALICE, BOB, request("ACK", "c1", "t1", "1 ACK", "")),
        dg(4_000, ALICE, BOB, then),
        dg(5_000, ALICE, BOB, request("BYE", "c1", "t1", "3 BYE", "")),
        dg(6_000, BOB, ALICE, response("200 OK", "c1", "t1", "3 BYE")),
    ])
}

fn occasions(doc: &FlowsDoc, rule: &str) -> u64 {
    scan(doc).population.get(rule).map_or(0, |p| p.occasions)
}

fn marked(doc: &FlowsDoc) -> Vec<(&str, bool)> {
    doc.legs.iter().map(|l| (l.call_id.as_str(), l.opening_request_set_aside)).collect()
}

#[test]
fn a_leg_whose_invite_was_set_aside_opens_no_dialog_or_transaction_occasion() {
    let doc = cancelled_call("");
    assert_eq!(doc.flow_stats.parse_failed, 1, "the INVITE is refused");
    assert_eq!(marked(&doc), [("c1", true)], "the leg is marked");
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
    let doc = cancelled_call(CONTACT);
    assert_eq!(doc.flow_stats.parse_failed, 0);
    assert_eq!(marked(&doc), [("c1", false)]);
    let json = serde_json::to_value(&doc).expect("document serializes");
    assert!(json["legs"][0].get("opening_request_set_aside").is_none(), "absent when false");

    assert!(occasions(&doc, "response-cseq-matches-transaction") > 0);
    assert!(scan(&doc).hits.is_empty(), "{:?}", scan(&doc).hits);
}

/// A refused request inside the dialog, or one belonging to the INVITE's
/// transaction, opens nothing: the leg keeps every rule.
#[test]
fn a_refused_in_dialog_request_ack_or_cancel_marks_nothing() {
    let reinvite = request("INVITE", "c1", "t1", "2 INVITE", &format!("{CONTACT}{TWICE}"));
    let ack = request("ACK", "c1", "", "1 ACK", TWICE);
    let cancel = request("CANCEL", "c1", "", "1 CANCEL", TWICE);
    for (case, refused) in [("re-INVITE", reinvite), ("untagged ACK", ack), ("CANCEL", cancel)] {
        let doc = answered_call_then(refused);
        assert_eq!(doc.flow_stats.parse_failed, 1, "{case} is refused");
        assert_eq!(marked(&doc), [("c1", false)], "{case}");
    }
}

/// A refused response opens nothing.
#[test]
fn a_refused_response_marks_nothing() {
    let doc = doc(&[
        dg(1_000, ALICE, BOB, request("INVITE", "c1", "", "1 INVITE", CONTACT)),
        dg(2_000, BOB, ALICE, message("SIP/2.0 100 Trying", "c1", false, "", "1 INVITE", TWICE)),
        dg(3_000, BOB, ALICE, response("486 Busy Here", "c1", "t1", "1 INVITE")),
        dg(4_000, ALICE, BOB, request("ACK", "c1", "t1", "1 ACK", "")),
    ]);
    assert_eq!(doc.flow_stats.parse_failed, 1, "the 100 is refused");
    assert_eq!(marked(&doc), [("c1", false)]);
}

/// A refused INVITE marks its own Call-ID's leg and no other.
#[test]
fn a_refusal_on_another_call_id_marks_only_that_leg() {
    let doc = doc(&[
        dg(1_000, ALICE, BOB, request("INVITE", "c1", "", "1 INVITE", CONTACT)),
        dg(2_000, BOB, ALICE, response("486 Busy Here", "c1", "t1", "1 INVITE")),
        dg(3_000, ALICE, BOB, request("ACK", "c1", "t1", "1 ACK", "")),
        dg(4_000, ALICE, BOB, request("INVITE", "c2", "", "1 INVITE", "")),
        dg(5_000, BOB, ALICE, response("486 Busy Here", "c2", "t2", "1 INVITE")),
        dg(6_000, ALICE, BOB, request("ACK", "c2", "t2", "1 ACK", "")),
    ]);
    assert_eq!(marked(&doc), [("c1", false), ("c2", true)]);
}

/// A proxied call whose INVITE is read on Alice's hop and refused on the
/// proxy's: a copy was read, so the leg is not marked and its rules run.
#[test]
fn a_copy_read_on_another_hop_leaves_the_leg_unmarked() {
    let hop1 = |start: &str, to_tag: &str, cseq: &str, extra: &str| {
        message(start, "c1", false, to_tag, cseq, extra)
    };
    let hop2 = |start: &str, to_tag: &str, cseq: &str, extra: &str| {
        message(start, "c1", true, to_tag, cseq, extra)
    };
    let invite = "INVITE sip:bob@example.com SIP/2.0";
    let doc = doc(&[
        dg(1_000, ALICE, PROXY, hop1(invite, "", "1 INVITE", CONTACT)),
        dg(1_500, PROXY, BOB, hop2(invite, "", "1 INVITE", &format!("{CONTACT}{TWICE}"))),
        dg(2_000, BOB, PROXY, hop2("SIP/2.0 486 Busy Here", "t1", "1 INVITE", "")),
        dg(2_500, PROXY, ALICE, hop1("SIP/2.0 486 Busy Here", "t1", "1 INVITE", "")),
        dg(3_000, ALICE, PROXY, hop1("ACK sip:bob@example.com SIP/2.0", "t1", "1 ACK", "")),
    ]);
    assert_eq!(doc.flow_stats.parse_failed, 1, "the proxy's copy is refused");
    assert_eq!(marked(&doc), [("c1", false)]);
    assert!(occasions(&doc, "response-cseq-matches-transaction") > 0, "the leg is judged");
}
