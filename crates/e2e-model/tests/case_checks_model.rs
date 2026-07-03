//! The new model surface for load-side checks: the `allowViolations` field,
//! the shared shape-independent lint helper (`collect_case_blocks`), and the
//! raw-`(entries, anchors)` evaluation path (`evaluate_blocks_over` /
//! `evaluate_case_over`) — including `sent`-anchor resolution (a message whose
//! only receiver is the SUT).

use std::collections::BTreeMap;

use e2e_model::checks::{Bindings, evaluate_blocks_over, evaluate_case_over};
use e2e_model::model::{CheckSet, TestCase, collect_case_blocks, schemas};
use scenario_harness::{AnchorKeys, AnchorTag};
use sip_message::parser::custom::CustomParser;
use sip_message::SipParser;
use sip_net::RecordedSipEntry;

fn parse_case(json: &str) -> TestCase {
    serde_json::from_str(json).unwrap()
}

// ---------------------------------------------------------------------------
// allowViolations (the authored audit-waiver field)
// ---------------------------------------------------------------------------

#[test]
fn allow_violations_parses_defaults_empty_and_roundtrips() {
    let with = parse_case(
        r#"{ "id": "t", "compatibleShapes": ["basic_call"],
             "allowViolations": ["rfc3261.noContactOnBye"] }"#,
    );
    assert_eq!(with.allow_violations, vec!["rfc3261.noContactOnBye"]);

    let without = parse_case(r#"{ "id": "t", "compatibleShapes": ["basic_call"] }"#);
    assert!(without.allow_violations.is_empty(), "default = full audit");

    // Round-trip: the empty default is not serialized; a set value is.
    let json = serde_json::to_string(&without).unwrap();
    assert!(!json.contains("allowViolations"), "{json}");
    let json = serde_json::to_string(&with).unwrap();
    assert!(json.contains("allowViolations"), "{json}");
}

#[test]
fn the_test_case_schema_declares_allow_violations() {
    let (_, schema) =
        schemas().into_iter().find(|(stem, _)| *stem == "test-case").expect("test-case schema");
    let text = serde_json::to_string(&schema).unwrap();
    assert!(text.contains("allowViolations"), "schema misses allowViolations: {text}");
}

// ---------------------------------------------------------------------------
// collect_case_blocks (the shared shape-independent lints)
// ---------------------------------------------------------------------------

#[test]
fn collect_case_blocks_flattens_sets_and_reports_problems() {
    let set: CheckSet = serde_json::from_str(
        r#"{ "id": "s1", "blocks": [
              { "on": "bob.initialInvite",
                "checks": [ { "field": "from.uri", "op": "exists" } ] } ] }"#,
    )
    .unwrap();
    let sets: BTreeMap<String, CheckSet> = [("s1".to_string(), set)].into();

    // Clean: one inline block + one set block, zero problems.
    let ok = parse_case(
        r#"{ "id": "t", "compatibleShapes": ["basic_call"],
             "checkSets": ["s1"],
             "checks": [ { "on": "alice.answer",
                           "checks": [ { "field": "to.tag", "op": "exists" } ] } ] }"#,
    );
    let (blocks, problems) = collect_case_blocks(&ok, &sets);
    assert_eq!(blocks.len(), 2, "inline + set blocks");
    assert!(problems.is_empty(), "{problems:?}");

    // Dirty: unknown set id, non-canonical anchor, op/value incoherence —
    // ALL reported (the same lints validate_case runs, sans shape compat).
    let bad = parse_case(
        r#"{ "id": "t", "compatibleShapes": ["basic_call"],
             "checkSets": ["nope"],
             "checks": [
               { "on": "alice.bogusAnchor",
                 "checks": [ { "field": "to.tag", "op": "exists" } ] },
               { "on": "alice.answer",
                 "checks": [ { "field": "to.tag", "op": "eq" } ] } ] }"#,
    );
    let (_, problems) = collect_case_blocks(&bad, &sets);
    let text = problems.join("\n");
    assert!(text.contains("unknown check set \"nope\""), "{text}");
    assert!(text.contains("bogusAnchor"), "{text}");
    assert!(text.contains("requires a value"), "{text}");
}

// ---------------------------------------------------------------------------
// evaluate_*_over on raw (entries, anchors) — received AND sent anchors
// ---------------------------------------------------------------------------

const INVITE_AT_BOB: &str = "INVITE sip:bob@10.0.0.2:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5080;branch=z9hG4bK-b-leg\r\n\
Max-Forwards: 69\r\n\
From: <sip:alice@pool.example>;tag=a1\r\n\
To: <sip:bob@10.0.0.2>\r\n\
Call-ID: cid-1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:sut@10.0.0.9:5080>\r\n\
Content-Length: 0\r\n\r\n";

const REFER_FROM_BOB: &str = "REFER sip:sut@10.0.0.9:5080 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.2:5070;branch=z9hG4bK-refer\r\n\
Max-Forwards: 70\r\n\
From: <sip:bob@10.0.0.2>;tag=b1\r\n\
To: <sip:alice@pool.example>;tag=a1\r\n\
Call-ID: cid-1\r\n\
CSeq: 2 REFER\r\n\
Refer-To: <sip:charlie@10.0.0.3:5071>\r\n\
Content-Length: 0\r\n\r\n";

/// One received entry (SUT → bob INVITE) + one sent entry (bob → SUT REFER),
/// with their anchors as the load surface would tag them.
fn fixture() -> (Vec<RecordedSipEntry>, Vec<AnchorTag>) {
    let bob = "10.0.0.2:5070".parse().unwrap();
    let sut = "10.0.0.9:5080".parse().unwrap();
    let entries = vec![
        RecordedSipEntry {
            from: sut,
            to: bob,
            raw: INVITE_AT_BOB.as_bytes().to_vec(),
            sent_ms: 1,
            received_ms: Some(1),
            delivered: true,
            seq: 1,
        },
        RecordedSipEntry {
            from: bob,
            to: sut,
            raw: REFER_FROM_BOB.as_bytes().to_vec(),
            sent_ms: 2,
            received_ms: None,
            delivered: true, // external destination: left the recording's horizon
            seq: 2,
        },
    ];
    let parser = CustomParser::new();
    let keys = |raw: &str| AnchorKeys::from(&parser.parse(raw.as_bytes()).unwrap());
    let anchors = vec![
        AnchorTag {
            agent: "bob".into(),
            anchor: "initialInvite".into(),
            agent_addr: bob,
            keys: keys(INVITE_AT_BOB),
            sent: false,
        },
        AnchorTag {
            agent: "bob".into(),
            anchor: "refer".into(),
            agent_addr: bob,
            keys: keys(REFER_FROM_BOB),
            sent: true,
        },
    ];
    (entries, anchors)
}

#[test]
fn evaluate_blocks_over_resolves_received_and_sent_anchors() {
    let (entries, anchors) = fixture();
    let case = parse_case(
        r#"{ "id": "t", "compatibleShapes": ["refer"],
             "input": { "core": { "from": "sip:alice@pool.example" } },
             "checks": [
               { "on": "bob.initialInvite",
                 "checks": [ { "field": "from.uri", "op": "eq", "value": "${input.from}" } ] },
               { "on": "bob.refer",
                 "checks": [ { "field": "header(Refer-To)", "op": "regex",
                               "value": "charlie@" } ] } ] }"#,
    );
    let bindings = Bindings { input: &case.input, lb_vip: "10.0.0.9:5080".parse().unwrap() };
    let verdicts = evaluate_case_over(&case, &BTreeMap::new(), &entries, &anchors, &bindings);
    assert_eq!(verdicts.len(), 2, "{verdicts:?}");
    assert!(verdicts.iter().all(|v| v.passed), "{verdicts:?}");
    // The received anchor bound the RELAYED b-leg INVITE (to == bob), the sent
    // anchor the REFER bob emitted (from == bob) — not each other.
    assert_eq!(verdicts[0].actual.as_deref(), Some("sip:alice@pool.example"));
    assert_eq!(verdicts[1].actual.as_deref(), Some("<sip:charlie@10.0.0.3:5071>"));
}

#[test]
fn a_sent_anchor_does_not_resolve_received_only_and_vice_versa() {
    let (entries, anchors) = fixture();
    // Flip the tags' direction: the INVITE tag claims `sent` (bob never sent
    // it) and the REFER tag claims received (nothing delivered REFER to bob) —
    // both must fail loudly, not silently bind the other entry.
    let flipped: Vec<AnchorTag> =
        anchors.into_iter().map(|t| AnchorTag { sent: !t.sent, ..t }).collect();
    let case = parse_case(
        r#"{ "id": "t", "compatibleShapes": ["refer"],
             "checks": [
               { "on": "bob.initialInvite",
                 "checks": [ { "field": "from.uri", "op": "exists" } ] },
               { "on": "bob.refer",
                 "checks": [ { "field": "header(Refer-To)", "op": "exists" } ] } ] }"#,
    );
    let bindings = Bindings { input: &case.input, lb_vip: "10.0.0.9:5080".parse().unwrap() };
    let refs: Vec<&e2e_model::CheckBlock> = case.checks.iter().collect();
    let verdicts = evaluate_blocks_over(&refs, &entries, &flipped, &bindings);
    assert_eq!(verdicts.len(), 2, "{verdicts:?}");
    for v in &verdicts {
        assert!(!v.passed, "flipped-direction anchor must not resolve: {v:?}");
        assert!(v.detail.contains("matches its keys"), "{v:?}");
    }
}
