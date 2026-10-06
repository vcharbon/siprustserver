//! The CDR consumer's library half: what it counts and the `/metrics` body
//! it serves.

pub mod counting;

use metric_catalogue::Catalogue;

/// The `/metrics` catalogue of the consumer: its counters, then the
/// process-wide observability counters.
pub const CATALOGUE: Catalogue = Catalogue {
    binary: "cdr-consumer-runner",
    sections: &[counting::FAMILIES, observe::counters::FAMILIES],
};

/// The `/metrics` body: the consumer's counters, then the dropped log lines
/// and trace-admission denials (ADR-0026).
pub fn metrics_body(metrics: &counting::Metrics) -> String {
    let mut text = metrics.prometheus_text();
    text.push_str(&observe::counters::prometheus_text());
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use counting::{Metrics, Payload};

    #[test]
    fn the_body_holds_its_catalogue_exactly_in_both_modes() {
        for payload in [Payload::Json, Payload::Opaque] {
            let m = Metrics::default();
            let _ = m.count(payload, br#"{"created_at":1,"terminated_at":2}"#);
            let text = metrics_body(&m);
            if let Err(mismatches) = CATALOGUE.check(&text) {
                panic!("{mismatches:#?}\n{text}");
            }
        }
    }
}
