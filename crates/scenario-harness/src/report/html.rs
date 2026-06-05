//! Self-contained HTML report for a SIP scenario — now a thin projector onto the
//! SHARED [`seq_report`] renderer (the unification described in `seq-report`'s
//! crate docs). The SIP recording becomes a single-plane [`seq_report::SeqDoc`]
//! and `seq_report::render_html` draws the sequence diagram + per-message
//! expandable wire text + legend, exactly the same machinery the failover
//! harness uses for its three-plane view.
//!
//! The previous bespoke SVG-embedding HTML lived here; the standalone SVG
//! artifact is still produced by [`super::svg`] (a different output format, kept
//! as-is). This file is now purely the SIP→`SeqDoc` adaptor for HTML output.

use layer_harness::RecordedScenario;
use seq_report::Anomaly;
use sip_net::RecordedSipEntry;

use super::project::sip_doc;

/// Render the whole report as one HTML document string via the shared renderer.
/// `extra_anomalies` carries the RFC 3261 CSeq hard-gate findings (if any), so a
/// trace that violates the rule renders the FAIL badge + lists the violation.
pub fn render(
    scenario_name: &str,
    description: Option<&str>,
    entries: &[RecordedSipEntry],
    scenario: &RecordedScenario,
    passed: bool,
    extra_anomalies: &[Anomaly],
) -> String {
    let doc = sip_doc(scenario_name, description, entries, scenario, passed, extra_anomalies);
    seq_report::render_html(&doc)
}
