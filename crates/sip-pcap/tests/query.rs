//! Query-engine contract: the transaction join, the projection vocabulary,
//! and loud rejection of a malformed query.
//!
//! The load-bearing test here is `transaction_scope_is_what_makes_the_join`:
//! it is the whole reason the transaction view exists, and a regression that
//! flattened scoping would still pass every other test in this file.

use serde_json::json;
use sip_pcap::flow::{build_flows, FlowConfig};
use sip_pcap::query::{select_groups, summary_row, KeyField, Query};
use sip_pcap::Datagram;

fn dg(ts_us: u64, src: &str, dst: &str, payload: &[u8]) -> Datagram {
    Datagram {
        ts_us,
        src: src.parse().unwrap(),
        dst: dst.parse().unwrap(),
        payload: payload.to_vec(),
        probe: 0,
    }
}

fn req(method: &str, call_id: &str, cseq: u32, branch: &str, to_tag: &str) -> Vec<u8> {
    format!(
        "{method} sip:+33123456789@example.net SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK{branch}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:+33999@caller.example>;tag=f1\r\n\
         To: <sip:+33123456789@example.net>{to_tag}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} {method}\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn resp(status: u16, call_id: &str, cseq: u32, method: &str, branch: &str, body: &str) -> Vec<u8> {
    format!(
        "SIP/2.0 {status} X\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK{branch}\r\n\
         From: <sip:+33999@caller.example>;tag=f1\r\n\
         To: <sip:+33123456789@example.net>;tag=t1\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} {method}\r\n\
         {}Content-Length: {}\r\n\r\n{body}",
        if body.is_empty() { "" } else { "Content-Type: application/sdp\r\n" },
        body.len()
    )
    .into_bytes()
}

fn query(v: serde_json::Value) -> Query {
    Query::from_json(&v).expect("query parses")
}

/// A call whose UPDATE was accepted but whose re-INVITE was rejected. Both a
/// 4xx and an UPDATE are present, so the flattened reading matches — and is
/// wrong.
fn update_ok_reinvite_rejected() -> Vec<Datagram> {
    let cid = "scope-1";
    vec![
        dg(1_000_000, "10.0.0.1:5060", "10.0.0.9:5060", &req("INVITE", cid, 1, "b1", "")),
        dg(1_500_000, "10.0.0.9:5060", "10.0.0.1:5060", &resp(200, cid, 1, "INVITE", "b1", "")),
        // UPDATE — accepted.
        dg(2_000_000, "10.0.0.1:5060", "10.0.0.9:5060", &req("UPDATE", cid, 2, "b2", ";tag=t1")),
        dg(2_100_000, "10.0.0.9:5060", "10.0.0.1:5060", &resp(200, cid, 2, "UPDATE", "b2", "")),
        // re-INVITE — rejected.
        dg(3_000_000, "10.0.0.1:5060", "10.0.0.9:5060", &req("INVITE", cid, 3, "b3", ";tag=t1")),
        dg(3_200_000, "10.0.0.9:5060", "10.0.0.1:5060", &resp(488, cid, 3, "INVITE", "b3", "")),
    ]
}

/// The same call with the roles swapped: the UPDATE is the rejected one.
fn update_rejected() -> Vec<Datagram> {
    let cid = "scope-2";
    vec![
        dg(1_000_000, "10.0.0.1:5060", "10.0.0.9:5060", &req("INVITE", cid, 1, "c1", "")),
        dg(1_500_000, "10.0.0.9:5060", "10.0.0.1:5060", &resp(200, cid, 1, "INVITE", "c1", "")),
        dg(2_000_000, "10.0.0.1:5060", "10.0.0.9:5060", &req("UPDATE", cid, 2, "c2", ";tag=t1")),
        dg(2_100_000, "10.0.0.9:5060", "10.0.0.1:5060", &resp(488, cid, 2, "UPDATE", "c2", "")),
    ]
}

/// THE reason the transaction view exists. "An UPDATE" and "a 4xx" as
/// independent leaves match a call where the UPDATE succeeded and something
/// else failed; scoped to one transaction they do not.
#[test]
fn transaction_scope_is_what_makes_the_join() {
    let flat = query(json!({
        "select": {"all": [{"method": "UPDATE"}, {"status": "4xx"}]}
    }));
    let scoped = query(json!({
        "select": {"any_txn": {"all": [{"method": "UPDATE"},
                                       {"final_status": {"ge": 400}}]}}
    }));

    let decoy = build_flows(&update_ok_reinvite_rejected(), &FlowConfig::default());
    assert_eq!(select_groups(&decoy, &flat).len(), 1, "the flat reading is fooled — that is the point");
    assert_eq!(
        select_groups(&decoy, &scoped).len(),
        0,
        "the rejected transaction is the re-INVITE, not the UPDATE"
    );

    let real = build_flows(&update_rejected(), &FlowConfig::default());
    assert_eq!(select_groups(&real, &flat).len(), 1);
    assert_eq!(select_groups(&real, &scoped).len(), 1, "the genuine case must still match");
}

/// A re-INVITE answered 200 with specific SDP: the body predicate must bind to
/// the SAME transaction's final response, not to any 200 in the call.
#[test]
fn reinvite_answer_body_binds_to_its_own_transaction() {
    let cid = "sdp-1";
    let datagrams = vec![
        dg(1_000_000, "10.0.0.1:5060", "10.0.0.9:5060", &req("INVITE", cid, 1, "d1", "")),
        // The INITIAL INVITE's 200 carries the marker …
        dg(1_500_000, "10.0.0.9:5060", "10.0.0.1:5060", &resp(200, cid, 1, "INVITE", "d1", "a=sendonly")),
        // … the re-INVITE's does not.
        dg(3_000_000, "10.0.0.1:5060", "10.0.0.9:5060", &req("INVITE", cid, 3, "d3", ";tag=t1")),
        dg(3_200_000, "10.0.0.9:5060", "10.0.0.1:5060", &resp(200, cid, 3, "INVITE", "d3", "a=sendrecv")),
    ];
    let flows = build_flows(&datagrams, &FlowConfig::default());
    let q = query(json!({
        "select": {"any_txn": {"all": [
            {"kind": "reinvite"},
            {"any_response": {"all": [{"status": 200}, {"body": {"contains": "a=sendonly"}}]}}
        ]}}
    }));
    assert_eq!(
        select_groups(&flows, &q).len(),
        0,
        "the marker is on the initial INVITE's answer, not the re-INVITE's"
    );

    // Move the marker onto the re-INVITE's answer and it matches.
    let mut moved = datagrams.clone();
    moved[1] = dg(1_500_000, "10.0.0.9:5060", "10.0.0.1:5060", &resp(200, cid, 1, "INVITE", "d1", "a=sendrecv"));
    moved[3] = dg(3_200_000, "10.0.0.9:5060", "10.0.0.1:5060", &resp(200, cid, 3, "INVITE", "d3", "a=sendonly"));
    let flows = build_flows(&moved, &FlowConfig::default());
    assert_eq!(select_groups(&flows, &q).len(), 1);
}

/// "Took longer than X to reject" — a timing predicate scoped to the
/// transaction that was rejected.
#[test]
fn latency_selects_a_slow_rejection() {
    let cid = "slow-1";
    let slow = vec![
        dg(1_000_000, "10.0.0.1:5060", "10.0.0.9:5060", &req("INVITE", cid, 1, "e1", "")),
        dg(6_000_000, "10.0.0.9:5060", "10.0.0.1:5060", &resp(486, cid, 1, "INVITE", "e1", "")),
    ];
    let q = query(json!({
        "select": {"any_txn": {"all": [{"kind": "initial_invite"},
                                       {"final_status": {"ge": 400}},
                                       {"latency_us": {"ge": 2000000}}]}}
    }));
    let flows = build_flows(&slow, &FlowConfig::default());
    assert_eq!(select_groups(&flows, &q).len(), 1, "5s to a 486 is a slow rejection");

    let fast = vec![
        dg(1_000_000, "10.0.0.1:5060", "10.0.0.9:5060", &req("INVITE", cid, 1, "e1", "")),
        dg(1_100_000, "10.0.0.9:5060", "10.0.0.1:5060", &resp(486, cid, 1, "INVITE", "e1", "")),
    ];
    let flows = build_flows(&fast, &FlowConfig::default());
    assert_eq!(select_groups(&flows, &q).len(), 0, "100ms is not");
}

/// `"none"` means no final response was ever seen — the timeout query. A leg
/// that never sent an INVITE must not satisfy it.
#[test]
fn no_final_response_is_distinguishable_from_no_invite() {
    let cid = "to-1";
    let timed_out = vec![
        dg(1_000_000, "10.0.0.1:5060", "10.0.0.9:5060", &req("INVITE", cid, 1, "f1", "")),
        dg(1_100_000, "10.0.0.9:5060", "10.0.0.1:5060", &resp(100, cid, 1, "INVITE", "f1", "")),
    ];
    let q = query(json!({"select": {"final_status": "none"}}));
    let flows = build_flows(&timed_out, &FlowConfig::default());
    assert_eq!(select_groups(&flows, &q).len(), 1);

    let options_only = vec![dg(
        1_000_000,
        "10.0.0.1:5060",
        "10.0.0.9:5060",
        &req("OPTIONS", "opt-1", 1, "g1", ""),
    )];
    let flows = build_flows(&options_only, &FlowConfig::default());
    assert_eq!(select_groups(&flows, &q).len(), 0, "a call that never was is not a timeout");
}

/// The projection vocabulary is the summary row's AND the neighbour key's —
/// `ruri_user` is host-insensitive so grouping by callee survives a rewrite.
#[test]
fn projection_yields_the_named_fields() {
    let flows = build_flows(&update_rejected(), &FlowConfig::default());
    let row = summary_row(&flows, 0, &[KeyField::CallId, KeyField::RuriUser, KeyField::FinalStatus]);
    assert_eq!(row["call_id"], json!(["scope-2"]));
    assert_eq!(row["ruri_user"], json!(["+33123456789"]), "host and params stripped");
    assert_eq!(row["final"], json!([200]));
}

/// Neighbours: same key, inside the window, the hit itself excluded.
#[test]
fn neighbours_expand_a_hit_by_the_named_key() {
    let mut datagrams = update_rejected();
    // Two more calls to the same callee: one nearby, one far outside the window.
    for (cid, t0, branch) in [("near-1", 10_000_000u64, "h1"), ("far-1", 900_000_000, "h2")] {
        datagrams.push(dg(t0, "10.0.0.1:5060", "10.0.0.9:5060", &req("INVITE", cid, 1, branch, "")));
        datagrams.push(dg(t0 + 100_000, "10.0.0.9:5060", "10.0.0.1:5060", &resp(200, cid, 1, "INVITE", branch, "")));
    }
    let flows = build_flows(&datagrams, &FlowConfig::default());
    let q = query(json!({
        "select": {"any_txn": {"all": [{"method": "UPDATE"}, {"final_status": {"ge": 400}}]}},
        "neighbours": {"key": ["ruri_user"], "window_us": 60000000}
    }));
    let hits = select_groups(&flows, &q);
    assert_eq!(hits.len(), 1);
    let spec = q.neighbours.as_ref().expect("declared");
    let near = sip_pcap::query::neighbours_of(&flows, &hits, spec);
    assert_eq!(near.len(), 1);
    assert_eq!(near[0].1.len(), 1, "the far call is outside the 60s window");
}

/// A misspelled predicate is an error, never a silently wider match set.
#[test]
fn malformed_queries_are_rejected_with_a_path() {
    let cases = [
        (json!({"select": {"any_txn": {"methd": "UPDATE"}}}), "unknown predicate"),
        (json!({"selct": {}}), "unknown key"),
        (json!({"select": {"method": "UPDATE", "status": 200}}), "exactly one key"),
        (json!({"project": {"mode": "summary", "fields": ["nope"]}}), "unknown field"),
        (json!({"select": {"kind": "not_a_kind"}}), "unknown transaction kind"),
        (json!({"version": 9}), "unsupported query version"),
        (json!({"select": {"latency_us": {}}}), "at least one bound"),
    ];
    for (doc, want) in cases {
        let err = Query::from_json(&doc).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(want), "expected {want:?} in {msg:?}");
        assert!(msg.starts_with('$'), "error must name its path: {msg}");
    }
}

/// An absent `select` matches everything: a query that only projects is a
/// legitimate (and common) screening query.
#[test]
fn a_query_without_a_predicate_selects_every_call() {
    let flows = build_flows(&update_rejected(), &FlowConfig::default());
    let q = query(json!({"project": {"mode": "count"}}));
    assert_eq!(select_groups(&flows, &q).len(), flows.groups.len());
}
