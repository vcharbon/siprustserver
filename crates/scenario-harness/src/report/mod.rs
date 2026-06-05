//! Report renderers — port of the `*-report` / `svg-sequence-diagram` family
//! in `src/test-harness/framework`. Every renderer consumes the **recording**:
//! the `RecordedSipEntry` trace projected by `sip_net::to_sip_entries` plus the
//! `RecordedScenario` (lanes + anomalies) drained from the `layer-harness`
//! `Recorder`. Nothing here reads interpreter state — the record is the source
//! of truth, as the migration's recording-first design intends.

pub mod html;
pub mod project;
pub mod svg;
pub mod text;
pub mod wire;

use std::path::{Path, PathBuf};

use crate::run::RunReport;

/// Render and write all three artifacts for a run under `out_dir`:
/// `<name>.svg`, `<name>.html`, `<name>.global.txt`, and `<net>/<agent>.txt`
/// per endpoint. Returns the paths written.
pub fn write_all(report: &RunReport, out_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let entries = report.entries();
    let scenario = report.scenario();
    let passed = report.passed();
    let name = &report.scenario_name;
    let desc = report.description.as_deref();

    // RFC 3261 status MUST reach the report: run the cross-message (CSeq) rule(s)
    // over the raw recording and fold any findings into the doc anomalies. A
    // violation forces the report to FAIL and lists the offending CSeq pair —
    // never PASS while the trace breaks the rule. (Only the cseq cross-message
    // rules; the structural close anomalies stay native to the recorder snapshot.)
    let extra_anomalies: Vec<seq_report::Anomaly> = sip_net::rfc_cross_message_rules()
        .iter()
        .flat_map(|rule| rule.check(report.events()))
        .map(|(lane, detail)| seq_report::Anomaly {
            check: "rfc3261.cseqInDialogOrder".to_string(),
            detail,
            lane: Some(lane),
        })
        .collect();

    std::fs::create_dir_all(out_dir)?;
    let mut written = Vec::new();

    let svg_doc = svg::render(&entries, &scenario.lanes, scenario.transport_kind);
    let svg_path = out_dir.join(format!("{name}.svg"));
    std::fs::write(&svg_path, svg_doc)?;
    written.push(svg_path);

    let html_doc = html::render(name, desc, &entries, &scenario, passed, &extra_anomalies);
    let html_path = out_dir.join(format!("{name}.html"));
    std::fs::write(&html_path, html_doc)?;
    written.push(html_path);

    let texts = text::render(name, desc, &entries, &scenario, passed, &extra_anomalies);
    written.extend(texts.write_to(out_dir)?);

    Ok(written)
}
