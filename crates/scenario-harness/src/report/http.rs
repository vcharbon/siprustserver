//! HTTP exchanges on the ladder.

use http_net::scripted::HttpFinding;
use http_net::RecordedHttpEntry;
use seq_report::Anomaly;

/// The report anomalies of scripted-service findings.
pub fn verdict_anomalies(
    _findings: &[HttpFinding],
    _entries: &[RecordedHttpEntry],
) -> Vec<Anomaly> {
    Vec::new()
}
