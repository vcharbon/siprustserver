//! Phase F acceptance (ADR-0018): an M1 run folds into a `RunResult` that
//! round-trips through `result.json`, a `campaign.json` aggregate indexes the
//! cells, and `seq_report::render_svg(&seq_doc)` produces the very diagram the
//! HTML report embeds.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use e2e_core::checks::{self, Bindings};
use e2e_core::model;
use e2e_core::result::{self, CampaignIndex, CellId, CellSummary, RunResult};
use e2e_core::{BasicCall, CallflowShape, EndpointConfig, FakeLsbcB2bua, InfraShape};

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn fake_cfg() -> EndpointConfig {
    let roles: BTreeMap<String, SocketAddr> = [
        ("alice", "127.0.0.1:5060"),
        ("bob1", "127.0.0.1:5070"),
        ("lb", "127.0.0.1:5080"),
        ("b2bua", "127.0.0.1:5090"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.parse().unwrap()))
    .collect();
    EndpointConfig {
        schema: None,
        infra_shape: "fake-lsbc-b2bua".into(),
        roles,
        recv_timeout_ms: 2_000,
        transit_delay_ms: 0,
    }
}

/// Run the committed case over the fake infra and fold it into a RunResult.
async fn produce_result() -> RunResult {
    let case = model::load_test_case(&workspace_root().join("e2e/cases/basic-call-identity.json"))
        .expect("committed case loads");
    let check_sets = model::load_check_sets(&workspace_root().join("e2e/checksets"))
        .expect("committed check sets load");
    let mut rt = FakeLsbcB2bua.build("result/fake", &fake_cfg()).await;
    let lb_vip = rt.lb_vip;
    BasicCall.run(&mut rt, &case.input.core).await;
    let (report, rfc_gate) = rt.finish().await;
    assert!(rfc_gate.is_empty(), "unexpected gating RFC findings: {rfc_gate:?}");

    let verdicts = checks::evaluate_case(
        &case,
        &check_sets,
        &report,
        &Bindings { input: &case.input, lb_vip },
    );
    let cell = CellId {
        case: case.id.clone(),
        shape: "basic-call".into(),
        infra: "fake-lsbc-b2bua".into(),
    };
    RunResult::from_run(cell, &report, verdicts, &[])
}

#[tokio::test(start_paused = true)]
async fn result_json_round_trips_and_campaign_indexes() {
    let result = produce_result().await;
    assert!(result.passed, "the committed case passes");
    assert!(!result.checks.is_empty());
    assert!(result.timings.messages > 0);
    assert!(result.timings.last_ms >= result.timings.first_ms);
    assert!(!result.seq_doc.rows.is_empty(), "the diagram has rows");
    assert_eq!(result.cell.dir_name(), "basic-call-identity__basic-call__fake-lsbc-b2bua");

    // Round-trip through the on-disk layout: runs_root/<campaign>/<ts>/<cell>/result.json.
    let runs_root =
        std::env::temp_dir().join(format!("e2e-result-test-{}", std::process::id()));
    let run_dir = result::run_dir(&runs_root, "smoke", "t0");
    let path = result::write_result(&run_dir, &result).expect("write result.json");
    assert!(path.ends_with(
        "smoke/t0/basic-call-identity__basic-call__fake-lsbc-b2bua/result.json"
    ));
    let back = result::read_result(path.parent().unwrap()).expect("read result.json back");
    assert_eq!(
        serde_json::to_value(&back).unwrap(),
        serde_json::to_value(&result).unwrap(),
        "result.json must round-trip losslessly"
    );

    // The aggregate index.
    let index = CampaignIndex {
        campaign: "smoke".into(),
        ts: "t0".into(),
        cells: vec![CellSummary {
            cell: result.cell.clone(),
            passed: result.passed,
            dir: result.cell.dir_name(),
            error: None,
        }],
    };
    result::write_campaign_index(&run_dir, &index).expect("write campaign.json");
    let index_back = result::read_campaign_index(&run_dir).expect("read campaign.json");
    assert_eq!(index_back, index);
    assert!(index_back.passed());

    std::fs::remove_dir_all(&runs_root).ok();
}

/// `render_svg` emits the EXACT diagram markup the HTML report embeds — one
/// projection, two surfaces.
#[tokio::test(start_paused = true)]
async fn render_svg_matches_the_html_reports_diagram() {
    let result = produce_result().await;
    let svg = seq_report::render_svg(&result.seq_doc);
    assert!(svg.starts_with("<svg"), "standalone SVG markup");
    let html = seq_report::render_html(&result.seq_doc);
    assert!(
        html.contains(&svg),
        "the HTML report must embed the same SVG render_svg returns"
    );
}
