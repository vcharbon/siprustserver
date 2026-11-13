//! `pivot-replay batch`'s inject lines: a cell whose document holds an
//! `inject` writes an `inject` line naming its run-spec, the node and the
//! action once the node is due, and resumes on the harness's answer line on
//! stdin. A refusal, or stdin closing before an answer, fails the cell's run
//! by name, the call still ended.

use std::io::{BufRead, BufReader, Lines, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use crate::plain_batch::{fixture, run_spec_at, scratch, stand_b2bua};

/// The action the injected cell names.
const ACTION: &str = "operation:restart-worker";

/// The `alice-calls-bob` cell with an `inject` of [`ACTION`] once the callee
/// took the ACK (`s9`), the caller's BYE (`s10`) ordered after it, as a case
/// directory under `dir`.
fn injected_case(dir: &Path) -> PathBuf {
    let source = fixture("alice-calls-bob");
    let case = dir.join("case");
    std::fs::create_dir_all(case.join("resources")).expect("the case directory");
    for entry in std::fs::read_dir(source.join("resources")).expect("the fixture's resources") {
        let entry = entry.expect("a resource");
        std::fs::copy(entry.path(), case.join("resources").join(entry.file_name()))
            .expect("a resource copied");
    }
    let text = std::fs::read_to_string(source.join("scenario.json")).expect("the scenario");
    let mut document: serde_json::Value = serde_json::from_str(&text).expect("a JSON document");
    let flow = document["flow"].as_array_mut().expect("a flow");
    let at = flow.iter().position(|node| node["id"] == "s10").expect("the BYE");
    flow[at]["after"] = serde_json::json!(["i1"]);
    flow.insert(
        at,
        serde_json::json!({ "id": "i1", "op": "inject", "action": ACTION, "after": ["s9"] }),
    );
    std::fs::write(case.join("scenario.json"), document.to_string()).expect("the scenario written");
    case
}

/// A batch process on the hosted lane, its stdin and stdout held.
struct Batch {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl Batch {
    /// The batch of `dir`, its ready line read.
    fn stand(dir: &Path) -> Batch {
        let spec = dir.join("batch-spec.json");
        let batch = serde_json::json!({ "lane": "hosted", "work_dir": dir });
        std::fs::write(&spec, batch.to_string()).expect("the batch-spec");
        let mut child = Command::new(env!("CARGO_BIN_EXE_pivot-replay"))
            .arg("batch")
            .arg(&spec)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("the batch process");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("its stdout")).lines();
        let mut batch = Batch { child, stdin, stdout };
        let ready = batch.line();
        assert!(ready["ready"].is_object(), "the ready line first: {ready}");
        batch
    }

    /// Writes `line` on the batch's stdin.
    fn say(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}").expect("a line written");
        stdin.flush().expect("flushed");
    }

    /// The next protocol line.
    fn line(&mut self) -> serde_json::Value {
        let line = self.stdout.next().expect("a line").expect("a readable line");
        serde_json::from_str(&line).expect("a JSON line")
    }

    /// Closes stdin; the end line once the batch ended.
    fn end(mut self) -> serde_json::Value {
        self.stdin = None;
        let end = self.line();
        assert!(self.child.wait().expect("it exits").success());
        end
    }
}

/// The injected cell's run-spec under `dir`, against the B2BUA at `ingress`;
/// its path and its bundle's.
fn injected_cell(dir: &Path, ingress: std::net::SocketAddr) -> (String, PathBuf) {
    let case = injected_case(dir);
    let out = dir.join("injected");
    let spec = out.with_extension("spec.json");
    std::fs::write(&spec, run_spec_at(&case, "hosted", ingress, &out).to_string())
        .expect("a run-spec");
    (spec.display().to_string(), out)
}

/// The failures of the bundle at `out`'s verdict.
fn failures(out: &Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(out.join("verdict.json")).expect("the cell's verdict");
    let verdict: serde_json::Value = serde_json::from_str(&text).expect("a JSON verdict");
    verdict["failures"].as_array().cloned().unwrap_or_default()
}

/// The injected cell run as a batch against a B2BUA on loopback UDP, the
/// harness answering its inject line with `answer` (the line's `spec`, the
/// node), or closing stdin where `answer` gives none; the cell line and the
/// cell's bundle.
fn run_injected(
    run: &str,
    answer: impl FnOnce(&str, &str) -> Option<String> + Send + 'static,
) -> (serde_json::Value, PathBuf) {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a runtime");
    let (h, b2bua) = stand_b2bua(&rt);
    let dir = scratch(run);
    let (spec, out) = injected_cell(&dir, b2bua.addr);
    let cell = rt.block_on(async {
        tokio::task::spawn_blocking(move || {
            let mut batch = Batch::stand(&dir);
            batch.say(&spec);
            let inject = batch.line();
            assert_eq!(
                inject,
                serde_json::json!({ "inject": { "spec": spec, "node": "i1", "action": ACTION } }),
                "the cell's inject line before its cell line"
            );
            match answer(&spec, "i1") {
                Some(line) => batch.say(&line),
                None => batch.stdin = None,
            }
            let cell = batch.line();
            batch.end();
            cell["cell"].clone()
        })
        .await
        .expect("the batch ran")
    });
    drop(h);
    (cell, out)
}

#[test]
fn an_answered_inject_line_resumes_the_cell_and_its_call_ends_as_scripted() {
    let (cell, out) = run_injected("inject-answered", |spec, node| {
        Some(serde_json::json!({ "injected": { "spec": spec, "node": node } }).to_string())
    });
    assert_eq!(cell["exit"], 0, "{cell}");
    assert_eq!(failures(&out), Vec::<serde_json::Value>::new());
}

#[test]
fn a_refused_inject_line_fails_the_cell_in_the_harness_s_words() {
    let (cell, out) = run_injected("inject-refused", |spec, node| {
        let refused = serde_json::json!({
            "refused": { "spec": spec, "node": node, "reason": "the operation exited 1" }
        });
        Some(refused.to_string())
    });
    assert_ne!(cell["exit"], 0, "{cell}");
    assert_eq!(
        failures(&out),
        [serde_json::json!({
            "failure": "inject-refused",
            "node": "i1",
            "action": ACTION,
            "reason": "the operation exited 1"
        })]
    );
}

#[test]
fn stdin_closing_before_an_answer_fails_the_cell_s_inject_by_name() {
    let (cell, out) = run_injected("inject-unanswered", |_, _| None);
    assert_ne!(cell["exit"], 0, "{cell}");
    assert_eq!(
        failures(&out),
        [serde_json::json!({
            "failure": "inject-refused",
            "node": "i1",
            "action": ACTION,
            "reason": "the harness closed the batch's stdin before it answered"
        })]
    );
}
