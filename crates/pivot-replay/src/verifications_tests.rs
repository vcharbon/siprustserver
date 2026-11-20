use std::path::PathBuf;

use pivot_schema::bundle::{Failure as Finding, RunVerdict};

use super::*;

const DRAIN: &str = "drain";
const DRAIN_SITE: &str = "post-run drain";
const RECORDS: &str = "records";
const RECORDS_REFERENCE: &str = "records-reference";

fn drained(failure: &Finding) -> bool {
    matches!(failure, Finding::CheckFailed { site, .. } if site == DRAIN_SITE)
}

fn records(failure: &Finding) -> bool {
    matches!(failure, Finding::CdrMismatch { .. })
}

/// A product's table: a drain gate charged at its site, a records gate with
/// its reference companion.
const TABLE: Gates = Gates {
    gates: &[
        Gate { name: DRAIN, source: "lane gate", charges: drained },
        Gate { name: RECORDS, source: "records-judge", charges: records },
    ],
    reference: Some(Reference { name: RECORDS_REFERENCE, gate: RECORDS }),
};

fn cell(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pivot-lines-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn written(dir: &Path) -> Vec<Line> {
    let lines = serde_json::from_str(&std::fs::read_to_string(dir.join(FILE)).unwrap()).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    lines
}

fn answers(lines: &[Line]) -> Vec<String> {
    lines.iter().map(|line| format!("{} {} {:?}", line.name, line.source, line.answer)).collect()
}

fn drain_breach() -> Finding {
    Finding::CheckFailed {
        site: DRAIN_SITE.into(),
        field: "sut.drained".into(),
        op: "drained".into(),
        expected: "no hold left".into(),
        observed: "1 hold left".into(),
    }
}

/// A failure charged at a registered gate's site fails that gate's line,
/// typed to the system under test, and leaves the call line passing.
#[test]
fn a_failure_at_a_registered_site_fails_its_gate_and_not_the_call() {
    let dir = cell("site");
    crate::verdict::write(&dir, &RunVerdict::ok("case", "lane"), &TABLE).unwrap();
    declare(&dir, &RunVerdict::ok("case", "lane"), &[(DRAIN, Standing::Pending)], &TABLE).unwrap();
    crate::verdict::charge(&dir, drain_breach(), &TABLE).unwrap();
    let lines = written(&dir);
    assert_eq!(answers(&lines), ["call interpreter Pass", "drain lane gate Fail"]);
    assert_eq!(lines[1].detail.as_ref().and_then(|d| d.side.as_deref()), Some("sut"));
}

/// With no gate registered, the same failure is the call's own.
#[test]
fn with_no_gate_registered_every_failure_is_the_calls() {
    let mut verdict = RunVerdict::ok("case", "lane");
    verdict.fail(drain_breach());
    let lines = lines_of(&verdict, &BTreeMap::new(), &Gates::NONE);
    assert_eq!(answers(&lines), ["call interpreter Fail"]);
    assert_eq!(lines[0].detail.as_ref().and_then(|d| d.side.as_deref()), None);
}

/// A reference left uncompared qualifies its gate's pass and stands beside
/// it, informative, under the gate's source; the batch's end settles a
/// pending gate and leaves an answered one as it is.
#[test]
fn a_reference_qualifies_its_gate_and_the_end_settles_only_a_pending_one() {
    let dir = cell("reference");
    crate::verdict::write(&dir, &RunVerdict::ok("case", "lane"), &TABLE).unwrap();
    let standing = [
        (DRAIN, Standing::Judged),
        (RECORDS, Standing::Pending),
        (RECORDS_REFERENCE, Standing::Unchecked("no reference stated".into())),
    ];
    declare(&dir, &RunVerdict::ok("case", "lane"), &standing, &TABLE).unwrap();
    let settled = [(DRAIN, Standing::Unchecked("late".into())), (RECORDS, Standing::Judged)];
    settle(&dir, &settled, &TABLE).unwrap();
    let lines = written(&dir);
    assert_eq!(
        answers(&lines),
        [
            "call interpreter Pass",
            "drain lane gate Pass",
            "records records-judge Pass",
            "records-reference records-judge NotCheckable",
        ]
    );
    assert!(lines[3].informative);
    assert_eq!(
        lines[2].message.as_deref(),
        Some("the reference was not compared: no reference stated")
    );
}
