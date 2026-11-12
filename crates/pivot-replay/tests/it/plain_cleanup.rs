//! The plain system's post-run cleanup gate reaches the bundle: a call the
//! worker still counts once the case settled fails `verdict.json` at the
//! cleanup site and the cell's `cleanup` line, not the exit code alone.

use std::path::{Path, PathBuf};

use pivot_replay::plain::PlainB2bua;
use pivot_replay::spec::RunSpec;
use pivot_replay::{execute_on, exit_code};
use pivot_schema::bundle::{Failure, RunVerdict, VerdictStatus};

/// The verdict site a reap breach is charged to, as the triage filters on
/// it: a literal, so a rename of the constant fails here.
const SITE: &str = "post-run cleanup";

/// The alice-calls-bob fixture on the simulated lane, its bundle under a
/// fresh `out` directory named for `run`.
fn spec(run: &str) -> RunSpec {
    let out =
        std::env::temp_dir().join(format!("pivot-replay-cleanup-{run}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).expect("the bundle directory");
    let case =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cells/alice-calls-bob");
    serde_json::from_value(serde_json::json!({
        "case": case,
        "out_dir": out,
        "lane": { "kind": "simulated", "egress_endpoint": "callee" },
        "run": { "timing_tolerance_ms": 20 }
    }))
    .expect("a run-spec")
}

fn verdict(out: &Path) -> RunVerdict {
    let text = std::fs::read_to_string(out.join("verdict.json")).expect("verdict.json");
    serde_json::from_str(&text).expect("a verdict")
}

/// The cell's lines as `(name, answer)`.
fn lines(out: &Path) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(out.join("verifications.json")).expect("verifications.json");
    let lines: Vec<serde_json::Value> = serde_json::from_str(&text).expect("a list of lines");
    lines
        .iter()
        .map(|line| {
            (
                line["name"].as_str().unwrap().to_string(),
                line["answer"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// The cleanup breaches the verdict carries.
fn cleanup_failures(verdict: &RunVerdict) -> Vec<String> {
    verdict
        .failures
        .iter()
        .filter_map(|f| match f {
            Failure::CheckFailed { site, observed, .. } if site == SITE => Some(observed.clone()),
            _ => None,
        })
        .collect()
}

/// The positive control: a reaped call passes both lines.
#[test]
fn a_reaped_call_passes_its_call_and_cleanup_lines() {
    let spec = spec("reaped");
    let result = execute_on(&spec, &PlainB2bua::default());
    assert_eq!(exit_code(&result), 0, "{:?}", result.as_ref().err());
    assert_eq!(verdict(&spec.out_dir).status, VerdictStatus::Ok);
    assert_eq!(
        lines(&spec.out_dir),
        [("call".to_string(), "pass".to_string()), ("cleanup".to_string(), "pass".to_string())]
    );
}

/// A call the worker counts and never removes leaves the wire exchange
/// intact: the reap check alone sees it, so the run exits failed (1), its
/// verdict fails at the cleanup site in the check's words, and its
/// `cleanup` line fails while its `call` line passes.
#[test]
fn a_worker_left_holding_a_call_fails_the_verdict_and_the_cleanup_line() {
    let spec = spec("unreaped");
    let result = execute_on(&spec, &PlainB2bua::leaking_a_call());
    assert_eq!(exit_code(&result), 1, "{:?}", result.as_ref().err());
    let verdict = verdict(&spec.out_dir);
    assert_eq!(verdict.status, VerdictStatus::Failed, "{:?}", verdict.failures);
    let breaches = cleanup_failures(&verdict);
    assert!(
        matches!(&breaches[..], [observed] if observed.contains("call leak: creations (2) != removals (1)")),
        "one breach at the cleanup site, stated once: {:?}",
        verdict.failures
    );
    assert_eq!(verdict.failures.len(), 1, "the reap check alone: {:?}", verdict.failures);
    assert_eq!(
        lines(&spec.out_dir),
        [("call".to_string(), "pass".to_string()), ("cleanup".to_string(), "fail".to_string())]
    );
}
