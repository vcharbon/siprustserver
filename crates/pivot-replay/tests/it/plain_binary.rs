//! The `pivot-replay` binary with nothing registered: each fixture cell runs
//! on the simulated lane against the plain system and passes, its bundle
//! holding the verdict, the call the worker terminated and the RFC audit; a
//! cell whose egress names the caller's own endpoint fails, since the plain
//! system dials the b-leg where the lane's egress points.

use std::path::{Path, PathBuf};
use std::process::Command;

use pivot_schema::bundle::{RunVerdict, VerdictStatus};

fn fixture(cell: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cells").join(cell)
}

/// The bundle directory of `run`, emptied.
fn out_dir(run: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pivot-replay-plain-{run}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the bundle directory");
    dir
}

/// Runs the binary on `cell` with its egress at `egress`, the bundle under
/// `out`; the exit code.
fn replay(cell: &str, egress: &str, out: &Path) -> i32 {
    let spec = serde_json::json!({
        "case": fixture(cell),
        "out_dir": out,
        "lane": { "kind": "simulated", "egress_endpoint": egress },
        "run": { "timing_tolerance_ms": 20 }
    });
    let spec_path = out.with_extension("spec.json");
    std::fs::write(&spec_path, spec.to_string()).expect("the run-spec");
    let status = Command::new(env!("CARGO_BIN_EXE_pivot-replay"))
        .arg(&spec_path)
        .status()
        .expect("the binary runs");
    status.code().expect("an exit code")
}

fn verdict(out: &Path) -> RunVerdict {
    let text = std::fs::read_to_string(out.join("verdict.json")).expect("verdict.json");
    serde_json::from_str(&text).expect("a verdict")
}

fn terminated(out: &Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(out.join("call.terminated.json")).expect("the calls");
    serde_json::from_str(&text).expect("a list of calls")
}

/// The lines of the cell's `call-records` export.
fn exported(out: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(out.join("exports/call-records.jsonl"))
        .expect("the call-records export")
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect()
}

/// The Call-ID of the first message the caller's leg `A` sent, as the wire
/// carried it.
fn caller_call_id(out: &Path) -> String {
    let text = std::fs::read_to_string(out.join("recording/A.jsonl")).expect("the caller's leg");
    let first: serde_json::Value =
        serde_json::from_str(text.lines().next().expect("a message")).expect("a JSON line");
    let raw = first["raw"].as_str().expect("the message's text");
    let line = raw.lines().find_map(|l| l.strip_prefix("Call-ID: ")).expect("a Call-ID");
    line.trim().to_string()
}

#[test]
fn the_plain_system_exports_one_call_record_per_call_keyed_by_the_callers_call_id() {
    for (cell, run) in [("alice-calls-bob", "records-basic"), ("bob-is-busy", "records-busy")] {
        let out = out_dir(run);
        assert_eq!(replay(cell, "callee", &out), 0, "{cell}");
        let lines = exported(&out);
        assert_eq!(lines.len(), 1, "{cell}: one record for its one call: {lines:?}");
        let call_id = caller_call_id(&out);
        assert_eq!(lines[0]["key"], call_id.as_str(), "{cell}: joined on the caller's Call-ID");
        assert_eq!(lines[0]["record"]["a_leg"]["call_id"], call_id.as_str(), "{cell}");
        assert_eq!(lines[0]["record"]["b_legs"].as_array().map(Vec::len), Some(1), "{cell}");
    }
}

#[test]
fn a_basic_call_passes_on_the_plain_system() {
    let out = out_dir("basic");
    assert_eq!(replay("alice-calls-bob", "callee", &out), 0);
    assert_eq!(verdict(&out).status, VerdictStatus::Ok);
    assert_eq!(terminated(&out).len(), 1, "the worker terminated the one call");
    assert!(out.join("rfc.json").exists(), "the RFC audit is in the bundle");
}

#[test]
fn a_call_the_callee_refuses_passes_on_the_plain_system() {
    let out = out_dir("busy");
    assert_eq!(replay("bob-is-busy", "callee", &out), 0);
    assert!(
        matches!(verdict(&out).status, VerdictStatus::Ok | VerdictStatus::OkNegative),
        "{:?}",
        verdict(&out).status
    );
    assert_eq!(terminated(&out).len(), 1);
}

#[test]
fn a_call_dialled_back_at_its_caller_fails() {
    let out = out_dir("misrouted");
    assert_eq!(replay("alice-calls-bob", "caller", &out), 1);
    assert_eq!(verdict(&out).status, VerdictStatus::Failed);
}
