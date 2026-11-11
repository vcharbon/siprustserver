//! A finished run folds into the `RunResult` a cell's `result.json` holds: the
//! verdict judges the run's expects, the check verdicts and the RFC hard gate
//! together; `rfc` republishes the rule-sourced findings only; the record
//! serializes in the camelCase form the TypeScript mirror decodes.

use e2e_core::{CellId, RunResult};
use e2e_model::checks::CheckVerdict;
use e2e_model::model::CheckOp;
use scenario_harness::{Harness, RunReport};
use sip_net_testkit::RfcFinding;

const SDP_OFFER: &str = "v=0\r\no=alice 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
const SDP_ANSWER: &str = "v=0\r\no=bob 2890 2890 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";

/// A full INVITE/180/200/ACK/BYE/200 call from alice to bob, closed through
/// the audited `finish()`.
async fn clean_call(name: &str) -> RunReport {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let mut call = alice.invite(&bob).with_sdp(SDP_OFFER).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(SDP_ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let report = h.finish().await;
    assert!(report.passed(), "{:?}", report.extra_anomalies);
    report
}

fn cell() -> CellId {
    CellId { case: "alice-calls-bob".into(), shape: "rust-test".into(), infra: "fake".into() }
}

fn verdict(passed: bool) -> CheckVerdict {
    CheckVerdict {
        on: "bob.initialInvite".into(),
        field: "from.uri".into(),
        op: CheckOp::Exists,
        expected: None,
        actual: Some("sip:alice@127.0.0.1:5060".into()),
        passed,
        detail: String::new(),
    }
}

#[tokio::test(start_paused = true)]
async fn a_clean_call_folds_into_a_passing_record_of_the_whole_dialog() {
    let report = clean_call("result-clean").await;
    let result = RunResult::from_run(cell(), &report, vec![verdict(true)], &[]);
    assert!(result.passed, "a clean call with passing checks passes");
    assert_eq!(result.timings.messages, 6, "the six messages of the dialog");
    assert!(result.timings.last_ms >= result.timings.first_ms);
    assert!(!result.seq_doc.rows.is_empty(), "the diagram has rows");
    assert!(result.rfc.iter().all(|a| a.rule_sourced), "{:?}", result.rfc);
}

#[tokio::test(start_paused = true)]
async fn the_record_is_camel_case_and_round_trips() {
    let report = clean_call("result-json").await;
    let result = RunResult::from_run(cell(), &report, vec![verdict(true)], &[]);
    let json = serde_json::to_value(&result).unwrap();
    let keys: Vec<&str> = json.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["cell", "checks", "passed", "rfc", "seqDoc", "timings"]);
    let timing_keys: Vec<&str> =
        json["timings"].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(timing_keys, ["firstMs", "lastMs", "messages"]);
    assert_eq!(json["cell"]["case"], "alice-calls-bob");

    let back: RunResult = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(serde_json::to_value(&back).unwrap(), json, "result.json round-trips losslessly");
}

#[tokio::test(start_paused = true)]
async fn a_failed_check_fails_the_cell() {
    let report = clean_call("result-failed-check").await;
    let result = RunResult::from_run(cell(), &report, vec![verdict(true), verdict(false)], &[]);
    assert!(!result.passed);
    assert_eq!(result.checks.len(), 2, "every verdict is kept");
}

#[tokio::test(start_paused = true)]
async fn a_gating_rfc_finding_fails_the_cell() {
    let report = clean_call("result-rfc-gate").await;
    let gate = [RfcFinding {
        rule: "some-rule".into(),
        lane: "bob".into(),
        detail: "a gating violation".into(),
        advisory: false,
        offending: None,
        charged: None,
    }];
    let result = RunResult::from_run(cell(), &report, Vec::new(), &gate);
    assert!(!result.passed, "the RFC hard gate fails the cell with its report intact");
    assert!(!result.seq_doc.rows.is_empty());
}

/// A harness finding the run carries (an HTTP verdict, here) reaches the
/// diagram's anomaly list and fails the cell, and never the RFC findings.
#[tokio::test(start_paused = true)]
async fn rfc_keeps_only_rule_sourced_findings() {
    let mut report = clean_call("result-rule-sourced").await;
    report.extra_anomalies.push(seq_report::Anomaly {
        check: "http.unmatched".into(),
        detail: "no script opens POST /x".into(),
        lane: None,
        endpoint: None,
        advisory: Some(false),
        row_seqs: Vec::new(),
        rule_sourced: false,
    });
    let result = RunResult::from_run(cell(), &report, Vec::new(), &[]);
    assert!(result.seq_doc.anomalies.iter().any(|a| a.check == "http.unmatched"));
    assert!(result.rfc.iter().all(|a| a.rule_sourced), "{:?}", result.rfc);
    assert!(!result.seq_doc.passed, "a gating harness finding fails the diagram");
    assert!(!result.passed, "and the cell");
}

/// `render_svg` over the record's `seqDoc` emits the exact diagram markup the
/// HTML report embeds.
#[tokio::test(start_paused = true)]
async fn the_seq_doc_renders_the_diagram_the_html_report_embeds() {
    let report = clean_call("result-svg").await;
    let result = RunResult::from_run(cell(), &report, Vec::new(), &[]);
    let svg = seq_report::render_svg(&result.seq_doc);
    assert!(svg.starts_with("<svg"), "standalone SVG markup");
    let html = seq_report::render_html(&result.seq_doc);
    assert!(html.contains(&svg), "the HTML report embeds the same SVG render_svg returns");
}
