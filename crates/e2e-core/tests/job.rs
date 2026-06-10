//! Phase G acceptance (ADR-0018): the Run Job executor expands a campaign to
//! cells, runs them under per-kind concurrency caps on per-cell current-thread
//! (paused for fake) runtimes, persists each cell, and aggregates — with a
//! failing check recorded as a failed cell and a cell PANIC recorded as a
//! crashed cell (`error.txt`), never killing the job. Plain `#[test]`s: the
//! executor owns its runtimes.

use std::path::PathBuf;

use e2e_core::model::{self, ModelError};
use e2e_core::run::{self, CampaignSpec};

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn temp_runs_root(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("e2e-job-test-{tag}-{}", std::process::id()))
}

/// Load the COMMITTED smoke campaign (campaign/case/infra files all from
/// `e2e/`) and run it end to end.
#[test]
fn committed_smoke_campaign_runs_green() {
    let e2e = workspace_root().join("e2e");
    let runs_root = temp_runs_root("smoke");
    let spec = run::load_spec(&e2e, &e2e.join("campaigns/smoke.json"), runs_root.clone(), "t0".into())
        .expect("committed smoke campaign loads");

    let result = run::run_blocking(&spec).expect("smoke campaign runs");
    assert!(result.passed(), "{:#?}", result.index);
    assert_eq!(result.index.cells.len(), 1);

    // The on-disk layout is populated.
    let cell_dir = result.run_dir.join(&result.index.cells[0].dir);
    assert!(cell_dir.join("result.json").is_file());
    let back = e2e_core::result::read_result(&cell_dir).unwrap();
    assert!(back.passed);
    let index = e2e_core::result::read_campaign_index(&result.run_dir).unwrap();
    assert_eq!(index, result.index);

    std::fs::remove_dir_all(&runs_root).ok();
}

/// Build a spec around the committed smoke files, then mutate it per test.
fn smoke_spec(tag: &str) -> CampaignSpec {
    let e2e = workspace_root().join("e2e");
    run::load_spec(&e2e, &e2e.join("campaigns/smoke.json"), temp_runs_root(tag), "t0".into())
        .expect("committed smoke campaign loads")
}

/// A failing CHECK yields a failed (not crashed) cell; a missing endpoint role
/// makes the cell PANIC, which is caught and recorded as crashed — and the
/// other cells still run to completion under a cap of 1.
#[test]
fn failing_and_crashing_cells_are_recorded_not_fatal() {
    let mut spec = smoke_spec("mixed");
    spec.campaign.concurrency.fake = 1;

    // Cell 2: same shape, but a check that cannot pass (wrong number).
    let mut failing = spec.cases.get("basic-call-identity").unwrap().clone();
    failing.id = "failing-check".into();
    failing.checks = serde_json::from_value(serde_json::json!([
        { "on": "bob1.initialInvite",
          "checks": [ { "field": "from.userInfo", "op": "regex", "value": "^\\+44" } ] }
    ]))
    .unwrap();
    spec.cases.insert("failing-check".into(), failing);
    spec.campaign.cases.push("failing-check".into());

    let result = run::run_blocking(&spec).expect("campaign completes despite failures");
    assert!(!result.passed());
    assert_eq!(result.index.cells.len(), 2);
    let by_case = |id: &str| {
        result.index.cells.iter().find(|c| c.cell.case == id).unwrap()
    };
    assert!(by_case("basic-call-identity").passed);
    let failed = by_case("failing-check");
    assert!(!failed.passed);
    assert!(failed.error.is_none(), "a failed check is a verdict, not a crash");
    // The failing cell still wrote a full result.json with the failing verdict.
    let failed_result =
        e2e_core::result::read_result(&result.run_dir.join(&failed.dir)).unwrap();
    assert!(failed_result.checks.iter().any(|v| !v.passed));

    std::fs::remove_dir_all(&spec.runs_root).ok();
}

#[test]
fn crashed_cell_writes_error_txt_and_fails_the_campaign() {
    let mut spec = smoke_spec("crash");
    // Drop bob1 from the endpoint config: the infra build panics in-cell.
    spec.endpoint_configs.get_mut("fake-lsbc-b2bua").unwrap().roles.remove("bob1");

    let result = run::run_blocking(&spec).expect("campaign completes despite the crash");
    assert!(!result.passed());
    let cell = &result.index.cells[0];
    assert!(!cell.passed);
    let err = cell.error.as_deref().expect("crash recorded on the summary");
    assert!(err.contains("bob1"), "panic message surfaced: {err}");
    assert!(result.run_dir.join(&cell.dir).join("error.txt").is_file());
    assert!(!result.run_dir.join(&cell.dir).join("result.json").exists());

    std::fs::remove_dir_all(&spec.runs_root).ok();
}

/// Expansion validates everything up front and reports precisely.
#[test]
fn expansion_problems_fail_before_anything_runs() {
    let mut spec = smoke_spec("invalid");
    spec.campaign.infra_shapes.push("no-such-infra".into());
    spec.campaign.cases.push("no-such-case".into());

    let Err(ModelError::Invalid(problems)) = run::run_blocking(&spec) else {
        panic!("expected validation failure");
    };
    let all = problems.join("\n");
    assert!(all.contains("unknown Infra shape \"no-such-infra\""), "{all}");
    assert!(all.contains("unknown Test case \"no-such-case\""), "{all}");
    assert!(
        !spec.runs_root.exists(),
        "nothing may be written when expansion fails"
    );
}

/// spawn_job: status reports progress and the join returns the aggregate.
#[test]
fn spawn_job_reports_progress_and_joins() {
    let spec = smoke_spec("job");
    let runs_root = spec.runs_root.clone();
    let handle = run::spawn_job(spec);
    let result = handle.join().expect("job completes");
    assert!(result.passed());

    std::fs::remove_dir_all(&runs_root).ok();
}

/// The smoke campaign also validates as data (sanity for `e2e validate`).
#[test]
fn committed_campaign_file_parses_with_schema_defaults() {
    let e2e = workspace_root().join("e2e");
    let campaign = model::load_campaign(&e2e.join("campaigns/smoke.json")).unwrap();
    assert_eq!(campaign.id, "smoke");
    assert_eq!(campaign.concurrency.fake, 8, "defaults applied");
    assert_eq!(campaign.concurrency.real, 1);
}
