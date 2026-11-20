//! `pivot-replay batch`'s inject lines: a cell whose document holds an
//! `inject` writes an `inject` line naming its run-spec, the node and the
//! action under an ask id of its own once the node is due, and resumes on the
//! harness's answer line on stdin echoing that ask. A refusal, or stdin closing before an
//! answer, fails the cell's run by name; whatever the answer, the call is
//! billed once and the system holds nothing of it at the batch's end.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Lines, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;

use crate::plain_batch::{answers, fixture, metrics_of, run_spec_at, scratch, stand_b2bua};

/// The action the injected cells name.
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
    /// The batch of `dir`, its end reading the system's `/metrics` at
    /// `metrics`, its ready line read.
    fn stand(dir: &Path, metrics: SocketAddr) -> Batch {
        let spec = dir.join("batch-spec.json");
        let batch = serde_json::json!({
            "lane": "hosted", "metrics": metrics.to_string(), "work_dir": dir
        });
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

    /// Closes stdin; each cell line by run-spec, and the end line, once the
    /// batch ended.
    fn end(mut self) -> (BTreeMap<String, serde_json::Value>, serde_json::Value) {
        self.stdin = None;
        let mut cells = BTreeMap::new();
        let end = loop {
            let line = self.line();
            if let Some(cell) = line.get("cell") {
                let spec = cell["spec"].as_str().expect("the cell's run-spec").to_string();
                cells.insert(spec, cell.clone());
                continue;
            }
            break line["end"].clone();
        };
        assert!(self.child.wait().expect("it exits").success());
        (cells, end)
    }
}

/// The injected cell named `name` under `dir`, of the case at `case`, against
/// the B2BUA at `ingress`: its run-spec's path and its bundle's.
fn injected_cell(dir: &Path, case: &Path, name: &str, ingress: SocketAddr) -> (String, PathBuf) {
    let out = dir.join(name);
    let spec = out.with_extension("spec.json");
    std::fs::write(&spec, run_spec_at(case, "hosted", ingress, &out).to_string())
        .expect("a run-spec");
    (spec.display().to_string(), out)
}

/// The failures of the bundle at `out`'s verdict.
fn failures(out: &Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(out.join("verdict.json")).expect("the cell's verdict");
    let verdict: serde_json::Value = serde_json::from_str(&text).expect("a JSON verdict");
    verdict["failures"].as_array().cloned().unwrap_or_default()
}

/// The answer of the line `name` among the bundle at `out`'s verifications.
fn answer_of(out: &Path, name: &str) -> String {
    answers(out)
        .into_iter()
        .find(|(line, _)| line == name)
        .map(|(_, answer)| answer)
        .unwrap_or_else(|| panic!("a {name} line in {}", out.display()))
}

/// What a batch of injected cells left: each cell line by run-spec, each
/// cell's bundle by run-spec, the end line, and the call records the system
/// wrote.
struct Ran {
    cells: BTreeMap<String, serde_json::Value>,
    bundles: BTreeMap<String, PathBuf>,
    end: serde_json::Value,
    records: usize,
}

impl Ran {
    /// The cell line and bundle of the cell whose run-spec is `spec`.
    fn cell(&self, spec: &str) -> (&serde_json::Value, &Path) {
        let line = self.cells.get(spec).unwrap_or_else(|| panic!("a cell line for {spec}"));
        (line, &self.bundles[spec])
    }

    /// Each call was billed once, the batch's end found the system holding
    /// none of them, and every cell's `cleanup` line passed.
    fn assert_calls_ended(&self) {
        assert_eq!(self.records, self.bundles.len(), "one call record per cell");
        assert_eq!(self.end["gates"][0]["name"], "cleanup", "{}", self.end);
        assert_eq!(self.end["gates"][0]["answer"], "pass", "{}", self.end);
        for out in self.bundles.values() {
            assert_eq!(answer_of(out, "cleanup"), "pass", "{}", out.display());
        }
    }
}

/// The injected cells `names` run as one batch against a B2BUA on loopback
/// UDP whose `/metrics` the batch's end reads; `converse` speaks with the batch
/// over its lines, given each cell's run-spec, then stdin closes. The B2BUA
/// holds nothing of any call once the batch ended.
fn in_batch(
    run: &str,
    names: &[&str],
    converse: impl FnOnce(&mut Batch, &[String]) + Send + 'static,
) -> Ran {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a runtime");
    let (h, b2bua) = stand_b2bua(&rt);
    let metrics = {
        let metrics = b2bua.metrics().clone();
        metrics_of(Arc::new(move || metrics.prometheus_text()))
    };
    let dir = scratch(run);
    let case = injected_case(&dir);
    let cells: Vec<(String, PathBuf)> =
        names.iter().map(|name| injected_cell(&dir, &case, name, b2bua.addr)).collect();
    let specs: Vec<String> = cells.iter().map(|(spec, _)| spec.clone()).collect();
    let (lines, end) = rt.block_on(async {
        tokio::task::spawn_blocking(move || {
            let mut batch = Batch::stand(&dir, metrics);
            converse(&mut batch, &specs);
            batch.end()
        })
        .await
        .expect("the batch ran")
    });
    rt.block_on(b2bua_harness::settle_until(|| b2bua.is_reaped()));
    b2bua.assert_fully_reaped();
    let records = b2bua.cdr_records().len();
    drop(h);
    Ran { cells: lines, bundles: cells.into_iter().collect(), end, records }
}

/// The one injected cell run as a batch, the harness answering its inject
/// line with `answer` (the line's ask), or closing stdin where `answer` gives
/// none; the cell line and the cell's bundle.
fn run_injected(
    run: &str,
    answer: impl FnOnce(&Ask) -> Option<String> + Send + 'static,
) -> (serde_json::Value, PathBuf) {
    let ran = in_batch(run, &["injected"], |batch, specs| {
        let spec = &specs[0];
        batch.say(spec);
        let inject = batch.line();
        let ask = Ask::of(&inject);
        assert_eq!(
            inject,
            serde_json::json!({ "inject": { "ask": ask.id, "spec": spec, "node": "i1", "action": ACTION } }),
            "the cell's inject line before its cell line"
        );
        if let Some(line) = answer(&ask) {
            batch.say(&line);
        }
    });
    ran.assert_calls_ended();
    let spec = ran.bundles.keys().next().expect("the cell").clone();
    let (cell, out) = ran.cell(&spec);
    (cell.clone(), out.to_path_buf())
}

/// What an inject line asks, as its answer echoes it: the batch's id of the
/// ask, the run-spec and the node.
struct Ask {
    id: String,
    spec: String,
    node: String,
}

impl Ask {
    /// The ask of the `inject` line `line`.
    fn of(line: &serde_json::Value) -> Ask {
        let field = |name: &str| {
            line["inject"][name].as_str().unwrap_or_else(|| panic!("{name} in {line}")).to_string()
        };
        Ask { id: field("ask"), spec: field("spec"), node: field("node") }
    }
}

/// The `injected` line answering `ask`.
fn injected(ask: &Ask) -> String {
    serde_json::json!({ "injected": { "ask": ask.id, "spec": ask.spec, "node": ask.node } })
        .to_string()
}

/// The `refused` line answering `ask` with `reason`.
fn refused(ask: &Ask, reason: &str) -> String {
    serde_json::json!({
        "refused": { "ask": ask.id, "spec": ask.spec, "node": ask.node, "reason": reason }
    })
    .to_string()
}

#[test]
fn an_answered_inject_line_resumes_the_cell_and_its_call_ends_as_scripted() {
    let (cell, out) = run_injected("inject-answered", |ask| Some(injected(ask)));
    assert_eq!(cell["exit"], 0, "{cell}");
    assert_eq!(failures(&out), Vec::<serde_json::Value>::new());
    assert_eq!(answer_of(&out, "call"), "pass");
}

#[test]
fn a_refused_inject_line_fails_the_cell_in_the_harness_s_words() {
    let (cell, out) =
        run_injected("inject-refused", |ask| Some(refused(ask, "the operation exited 1")));
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
    let (cell, out) = run_injected("inject-unanswered", |_| None);
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

/// Two cells' injects of the same node, answered in the reverse order of their
/// asking, each reach their own cell by run-spec; a line that does not decode
/// and an answer for a run-spec that asked nothing, said before them, change
/// nothing and run no cell.
#[test]
fn each_answer_line_reaches_the_inject_its_run_spec_asked() {
    let ran = in_batch("inject-routed", &["first", "second"], |batch, specs| {
        batch.say("{ not an answer");
        let unasked =
            Ask { id: "1".into(), spec: "/nowhere/asked.spec.json".into(), node: "i1".into() };
        batch.say(&injected(&unasked));
        for spec in specs {
            batch.say(spec);
        }
        let mut asked: Vec<Ask> = (0..2)
            .map(|_| {
                let inject = batch.line();
                assert_eq!(inject["inject"]["node"], "i1", "{inject}");
                Ask::of(&inject)
            })
            .collect();
        asked.reverse();
        for ask in asked {
            if ask.spec == specs[0] {
                batch.say(&injected(&ask));
            } else {
                batch.say(&refused(&ask, "the second operation exited 1"));
            }
        }
    });
    ran.assert_calls_ended();
    assert_eq!(ran.cells.len(), 2, "{:?}", ran.cells);
    let mut specs = ran.bundles.keys().cloned();
    let (first, second) = (specs.next().expect("first"), specs.next().expect("second"));
    let (line, out) = ran.cell(&first);
    assert_eq!(line["exit"], 0, "{line}");
    assert_eq!(failures(out), Vec::<serde_json::Value>::new());
    let (line, out) = ran.cell(&second);
    assert_ne!(line["exit"], 0, "{line}");
    assert_eq!(
        failures(out),
        [serde_json::json!({
            "failure": "inject-refused",
            "node": "i1",
            "action": ACTION,
            "reason": "the second operation exited 1"
        })]
    );
}
