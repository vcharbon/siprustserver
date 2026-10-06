//! A scripted HTTP service's findings as report anomalies, for
//! [`RunReport::extra_anomalies`](crate::RunReport::extra_anomalies).

use std::collections::HashSet;

use http_net::scripted::{HttpFinding, HttpFindingKind};
use http_net::{HttpOutcome, RecordedHttpEntry};
use seq_report::Anomaly;

/// The report anomalies of a scripted service's findings: gating unless the
/// finding is advisory, never RFC-rule-sourced, each linked to the request row
/// it refused (the served `500` answering the same request).
pub fn verdict_anomalies(findings: &[HttpFinding], entries: &[RecordedHttpEntry]) -> Vec<Anomaly> {
    let mut linked: HashSet<u64> = HashSet::new();
    findings
        .iter()
        .map(|f| {
            let row = entries.iter().find(|e| {
                !linked.contains(&e.seq)
                    && e.served
                    && e.request.method == f.method
                    && e.request.path == f.path
                    && String::from_utf8_lossy(&e.request.body) == f.body
                    && matches!(&e.outcome, Some(HttpOutcome::Response(r)) if r.status == 500)
            });
            if let Some(e) = row {
                linked.insert(e.seq);
            }
            let detail = if f.method.is_empty() {
                f.detail.clone()
            } else {
                format!("{} {}: {}", f.method, f.path, f.detail)
            };
            Anomaly {
                check: check_of(f.kind).to_string(),
                detail,
                lane: row.map(|e| e.service.clone()),
                endpoint: None,
                advisory: Some(f.is_advisory()),
                row_seqs: row.map(|e| vec![e.seq]).unwrap_or_default(),
                rule_sourced: false,
            }
        })
        .collect()
}

fn check_of(kind: HttpFindingKind) -> &'static str {
    match kind {
        HttpFindingKind::Unmatched => "http.unmatched",
        HttpFindingKind::Ambiguous => "http.ambiguous",
        HttpFindingKind::Unserved => "http.unserved",
        HttpFindingKind::ResetNotForwarded => "http.resetNotForwarded",
        HttpFindingKind::ForeignToken => "http.foreignToken",
    }
}
