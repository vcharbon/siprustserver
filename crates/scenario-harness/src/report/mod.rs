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

/// Project a finished run into the neutral [`seq_report::SeqDoc`] with the
/// pure RFC cross-message anomaly fold, for callers that persist it (the E2E
/// `result.json`, ADR-0018 Phase F — its `rfc` field republishes these
/// anomalies as RFC findings, so failed expects stay out) and draw it later
/// via `seq_report::render_svg`/`render_html`.
pub fn seq_doc(report: &RunReport) -> seq_report::SeqDoc {
    let entries = report.entries();
    let scenario = report.scenario();
    project::sip_doc(
        &report.scenario_name,
        report.description.as_deref(),
        &entries,
        &scenario,
        report.passed(),
        &cross_message_anomalies(report),
    )
}

/// The extra (non-recorder) anomalies the WRITTEN artifacts carry: the RFC
/// cross-message fold plus every FAILED `ExpectOutcome` rendered as a gating
/// anomaly — so the artifacts state WHY a run is `FAIL` (the Drop-path writer
/// pushes the panic message as a failed expect; a data-DSL mismatch lists what
/// was expected vs received). Passing outcomes add nothing. [`write_all`]-only:
/// [`seq_doc`] keeps the pure RFC fold, because its consumers (the E2E
/// `result.json` `rfc` field) publish the doc anomalies AS RFC findings.
fn doc_anomalies(report: &RunReport) -> Vec<seq_report::Anomaly> {
    let mut anomalies = cross_message_anomalies(report);
    anomalies.extend(report.expects.iter().filter(|e| !e.passed).map(|e| {
        seq_report::Anomaly {
            check: "expect".to_string(),
            detail: format!("[{}] expected {}: {}", e.agent, e.expected, e.detail),
            lane: None,
            endpoint: None, // the `[{agent}]` prefix in `detail` carries attribution
            advisory: Some(false),
            row_seqs: Vec::new(),
            rule_sourced: false, // a step's expect, not an audit rule
        }
    }));
    anomalies
}

/// RFC status reaches the report: fold the report's full-suite finding set
/// ([`RunReport::rfc_findings`] — the SHARED role-aware evaluator
/// `sip_net::evaluate_rfc_findings`, run once per report) into the doc
/// anomalies, each tagged with its rule name and its advisory/gating severity.
/// The evaluator applies **subject dispatch** — a finding is kept only when
/// the rule's `subject()` intersects the originating bind's declared roles —
/// so the report can no longer list a proxy-subject rule against a UA lane
/// (the e2e false-positive class). This is the same pass the `agent.rs` hard
/// gate panics on; the two can never disagree.
///
/// (For the `agent.rs` path the recorder also carries these rules natively, so
/// findings duplicate the recorder's ledger entries — the projector dedupes
/// them; for the `run.rs` path, whose recorder carries no rules, this fold is
/// the only source.)
fn cross_message_anomalies(report: &RunReport) -> Vec<seq_report::Anomaly> {
    // A finding's `offending` is a 1-based index into the AUDIT wire view;
    // resolve it to that entry's global `seq` — the stable row identity the
    // rendered doc keys on (the doc's rows are a superset view over the same
    // recording, so seq equality is the join).
    let wire = sip_net::audit_wire_entries(report.events());
    report
        .rfc_findings()
        .iter()
        .map(|f| seq_report::Anomaly {
            check: f.rule.clone(),
            detail: f.detail.clone(),
            lane: Some(f.lane.clone()),
            endpoint: None, // resolved against the recorder lanes by `sip_doc`
            advisory: Some(f.advisory),
            row_seqs: f
                .offending
                .and_then(|i| wire.get(i - 1))
                .map(|e| vec![e.seq])
                .unwrap_or_default(),
            rule_sourced: true, // straight off the audit registry
        })
        .collect()
}

/// Render and write all three artifacts for a run under `out_dir`:
/// `<name>.svg`, `<name>.html`, `<name>.global.txt`, and `<net>/<agent>.txt`
/// per endpoint. Returns the paths written.
pub fn write_all(report: &RunReport, out_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let entries = report.entries();
    let scenario = report.scenario();
    let passed = report.passed();
    let name = &report.scenario_name;
    let desc = report.description.as_deref();

    let extra_anomalies = doc_anomalies(report);

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
