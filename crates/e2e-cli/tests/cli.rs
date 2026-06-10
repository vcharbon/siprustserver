//! Phase H acceptance (ADR-0018): a green campaign exits 0; a campaign with a
//! failing check exits 1 with the failing cells listed; `validate` lints
//! authored docs; all through the REAL `cli()` surface the binary wraps.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn s(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| p.to_string()).collect()
}

#[test]
fn green_campaign_exits_zero() {
    let e2e = workspace_root().join("e2e");
    let runs_root = std::env::temp_dir().join(format!("e2e-cli-green-{}", std::process::id()));
    let code = e2e_cli::cli(&s(&[
        "run",
        e2e.join("campaigns/smoke.json").to_str().unwrap(),
        "--runs-root",
        runs_root.to_str().unwrap(),
        "--ts",
        "t0",
    ]));
    assert_eq!(code, 0);
    assert!(runs_root.join("smoke/t0/campaign.json").is_file());
    std::fs::remove_dir_all(&runs_root).ok();
}

/// A campaign whose case carries an impossible check: exit 1, cell persisted.
#[test]
fn failing_campaign_exits_one() {
    // Build a self-contained e2e dir: the committed case with a wrong-number
    // regex, the committed fake infra config, a one-case campaign.
    let root = std::env::temp_dir().join(format!("e2e-cli-fail-{}", std::process::id()));
    let e2e = root.join("e2e");
    for sub in ["cases", "campaigns", "infra", "checksets"] {
        std::fs::create_dir_all(e2e.join(sub)).unwrap();
    }
    let committed = workspace_root().join("e2e");
    let mut case: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(committed.join("cases/basic-call-identity.json")).unwrap(),
    )
    .unwrap();
    case["id"] = "wrong-number".into();
    case["checks"] = serde_json::json!([
        { "on": "bob1.initialInvite",
          "checks": [ { "field": "from.userInfo", "op": "regex", "value": "^\\+44" } ] }
    ]);
    case.as_object_mut().unwrap().remove("$schema");
    // This temp e2e dir has no check sets — drop the committed case's reference.
    case.as_object_mut().unwrap().remove("checkSets");
    std::fs::write(e2e.join("cases/wrong-number.json"), case.to_string()).unwrap();
    std::fs::copy(
        committed.join("infra/fake-lsbc-b2bua.json"),
        e2e.join("infra/fake-lsbc-b2bua.json"),
    )
    .unwrap();
    std::fs::write(
        e2e.join("campaigns/fail.json"),
        serde_json::json!({
            "id": "fail",
            "cases": ["wrong-number"],
            "infraShapes": ["fake-lsbc-b2bua"]
        })
        .to_string(),
    )
    .unwrap();

    let code = e2e_cli::cli(&s(&[
        "run",
        e2e.join("campaigns/fail.json").to_str().unwrap(),
        "--ts",
        "t0",
    ]));
    assert_eq!(code, 1, "a failing check must gate CI");
    // Default runs root is <e2e-dir>/runs; the failing cell's result persisted.
    let cell_dir = e2e.join("runs/fail/t0/wrong-number__basic-call__fake-lsbc-b2bua");
    assert!(cell_dir.join("result.json").is_file());
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn case_and_infra_filters_subset_and_reject_typos() {
    let e2e = workspace_root().join("e2e");
    let runs_root = std::env::temp_dir().join(format!("e2e-cli-filter-{}", std::process::id()));
    let campaign = e2e.join("campaigns/smoke.json");

    let code = e2e_cli::cli(&s(&[
        "run",
        campaign.to_str().unwrap(),
        "--case",
        "basic-call-identity",
        "--infra",
        "fake-lsbc-b2bua",
        "--runs-root",
        runs_root.to_str().unwrap(),
        "--ts",
        "t0",
    ]));
    assert_eq!(code, 0);

    let code = e2e_cli::cli(&s(&[
        "run",
        campaign.to_str().unwrap(),
        "--case",
        "no-such-case",
        "--runs-root",
        runs_root.to_str().unwrap(),
    ]));
    assert_eq!(code, 2, "a filter typo must not silently run nothing");
    std::fs::remove_dir_all(&runs_root).ok();
}

#[test]
fn validate_accepts_committed_docs_and_rejects_bad_ones() {
    let e2e = workspace_root().join("e2e");
    let code = e2e_cli::cli(&s(&[
        "validate",
        e2e.join("cases/basic-call-identity.json").to_str().unwrap(),
        e2e.join("campaigns/smoke.json").to_str().unwrap(),
        e2e.join("infra/fake-lsbc-b2bua.json").to_str().unwrap(),
        e2e.join("infra/real-loopback-direct.json").to_str().unwrap(),
    ]));
    assert_eq!(code, 0, "every committed authored doc lints clean");

    // An anchor basic-call does not publish: parses as a test case, fails deep
    // validation.
    let bad = std::env::temp_dir().join(format!("e2e-cli-bad-{}.json", std::process::id()));
    std::fs::write(
        &bad,
        serde_json::json!({
            "id": "bad",
            "compatibleShapes": ["basic-call"],
            "checks": [
                { "on": "bob1.prack", "checks": [ { "field": "rack", "op": "exists" } ] }
            ]
        })
        .to_string(),
    )
    .unwrap();
    let code = e2e_cli::cli(&s(&["validate", bad.to_str().unwrap()]));
    assert_eq!(code, 1);
    std::fs::remove_file(&bad).ok();
}

#[test]
fn unknown_subcommand_prints_usage() {
    assert_eq!(e2e_cli::cli(&s(&["bogus"])), 2);
    assert_eq!(e2e_cli::cli(&[]), 2);
}
