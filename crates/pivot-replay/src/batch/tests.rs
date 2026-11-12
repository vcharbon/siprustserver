use std::path::Path;

use pivot_schema::bundle::{Failure as Finding, RunRfcAudit, RunVerdict, VerdictStatus};
use serde::Deserialize;

use super::gates::Gate;
use super::*;
use crate::verifications::{self, Answer, Line, Reference, Standing};

const DRAIN: &str = "drain";
const DRAIN_SITE: &str = "post-run drain";

fn drained(failure: &Finding) -> bool {
    matches!(failure, Finding::CheckFailed { site, .. } if site == DRAIN_SITE)
}

static TABLE: Gates = Gates {
    gates: &[verifications::Gate { name: DRAIN, source: "lane gate", charges: drained }],
    reference: None::<Reference>,
};

/// A backend double nobody calls.
struct Silent(SocketAddr);

impl BackendDouble for Silent {
    type Row = u64;

    fn addr(&self) -> SocketAddr {
        self.0
    }

    fn rows(&self) -> Vec<u64> {
        vec![120, 80]
    }

    fn rows_of(&self, _: &BTreeSet<String>) -> Vec<u64> {
        Vec::new()
    }

    fn key_of(&self, call: &DialogIdentity) -> String {
        call.call_id.clone()
    }

    fn process_us(row: &u64) -> Option<u64> {
        Some(*row)
    }
}

#[derive(Deserialize)]
struct ToySpec {
    lane: String,
}

/// A product whose cells write a green verdict with the drain gate pending,
/// and whose end answers the drain gate clean.
struct Toy;

#[derive(Serialize)]
struct Report {
    note: &'static str,
}

impl Registered for Toy {
    type Spec = ToySpec;
    type Backend = Silent;
    type Report = Report;

    fn lane(spec: &ToySpec) -> &str {
        &spec.lane
    }

    fn gates() -> &'static Gates {
        &TABLE
    }

    async fn stand(_: &ToySpec, _: u64) -> Result<(Toy, Silent), Failure> {
        Ok((Toy, Silent("127.0.0.1:9".parse().unwrap())))
    }

    fn cell(host: &Host<Toy>, path: &Path, _: u64) -> Result<RunSummary, Failure> {
        if !path.is_dir() {
            return Err(Failure::Environment(format!("no cell at {}", path.display())));
        }
        let verdict = RunVerdict::ok("case", "toy");
        crate::verdict::write(path, &verdict, &TABLE)?;
        verifications::declare(path, &verdict, &[(DRAIN, Standing::Pending)], &TABLE)?;
        host.minted(path, &[]);
        Ok(RunSummary {
            verdict: VerdictStatus::Ok,
            failures: 0,
            cleanup: Ok(()),
            rfc: RunRfcAudit::NotAudited { reason: "a toy".into() },
            bundle: path.to_path_buf(),
        })
    }

    async fn end(_: &Host<Toy>, placed: &[Placed]) -> Ended<Report> {
        Ended {
            gates: vec![Gate::over(DRAIN, None)],
            charged: Vec::new(),
            settled: vec![(DRAIN, Standing::Judged)],
            report: Report { note: if placed.len() == 1 { "one placed" } else { "other" } },
        }
    }
}

/// A writer the test reads back.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The protocol end to end with a registered product: the ready line naming
/// the backend double, one line per cell whatever became of it, and the end
/// line carrying the registered gates and report; the end settles the line
/// each placed cell left pending.
#[test]
fn a_batch_runs_the_registered_cells_and_ends_with_their_gates() {
    let cell = std::env::temp_dir().join(format!("pivot-batch-cell-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cell);
    std::fs::create_dir_all(&cell).unwrap();
    let cells = format!("/nowhere/run-spec.json\n\n{}\n", cell.display());
    let captured = Captured::default();
    let out: Out = Arc::new(Mutex::new(Box::new(captured.clone())));
    let spec = ToySpec { lane: "toy".into() };
    serve::<Toy>(&spec, cells.as_bytes(), out, Process { nofile: "64".into(), signals: false })
        .expect("the batch ran");

    let text = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    let lines: Vec<serde_json::Value> =
        text.lines().map(|l| serde_json::from_str(l).expect("a JSON line")).collect();
    assert_eq!(lines.len(), 4, "{text}");
    assert_eq!(lines[0]["ready"]["bl"], "127.0.0.1:9");
    let exit_of = |spec: &str| {
        lines[1..3]
            .iter()
            .find(|l| l["cell"]["spec"] == spec)
            .and_then(|l| l["cell"]["exit"].as_i64())
    };
    assert_eq!(exit_of("/nowhere/run-spec.json"), Some(3));
    assert_eq!(exit_of(&cell.display().to_string()), Some(0));
    assert_eq!(
        lines[3]["end"],
        serde_json::json!({
            "dirty": null,
            "gates": [{ "name": DRAIN, "answer": "pass" }],
            "note": "one placed",
            "charged": [],
            "bl": { "served": 2, "process_us": { "p50": 80, "p99": 120, "max": 120 } },
        })
    );
    let written: Vec<Line> =
        serde_json::from_str(&std::fs::read_to_string(cell.join(verifications::FILE)).unwrap())
            .unwrap();
    std::fs::remove_dir_all(&cell).unwrap();
    let drain = written.iter().find(|line| line.name == DRAIN).expect("the drain line");
    assert_eq!(drain.answer, Answer::Pass, "the end settled the pending line");
}

/// A panic in a cell is that cell's own failure line, exit 4.
#[test]
fn a_panicking_cell_is_its_own_failure() {
    let line = line_of("/x/spec.json", || panic!("cell fault"));
    assert_eq!(line.exit, 4);
    assert!(line.summary.contains("cell fault"), "{}", line.summary);
}
