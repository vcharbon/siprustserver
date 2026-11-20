//! The CAP READINGS of one run, and the `caps.json` they land in beside
//! `verdict.json`: the largest reading of each per-call cap the SUT's landed
//! turns left (ADR-0057 §9), and of each cap on what grows with a
//! call's age per started hour of its granted duration. The largest over the
//! corpus sets the caps' defaults (ADR-0057 §9).

use std::collections::BTreeMap;
use std::path::Path;

use b2bua::config::Cap;
use b2bua::metrics::B2buaMetrics;
use serde::Serialize;

use crate::Failure;

/// The bundle file holding this run's cap readings.
pub const CAPS_FILE: &str = "caps.json";

/// One run's readings, by cap label; a cap read nothing is `0`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Readings {
    pub peak: BTreeMap<&'static str, u64>,
    pub per_hour: BTreeMap<&'static str, u64>,
}

/// The readings `metrics` holds.
pub fn of(metrics: &B2buaMetrics) -> Readings {
    let peak = Cap::ALL.into_iter().map(|cap| (cap.label(), metrics.cap_peak_of(cap))).collect();
    let per_hour = [Cap::Legs, Cap::Attempts, Cap::CdrEvents]
        .into_iter()
        .map(|cap| (cap.label(), metrics.cap_peak_per_hour_of(cap)))
        .collect();
    Readings { peak, per_hour }
}

/// Write `caps.json` into the bundle.
pub fn record(out_dir: &Path, metrics: &B2buaMetrics) -> Result<(), Failure> {
    let text = serde_json::to_string_pretty(&of(metrics))
        .map_err(|e| Failure::Environment(format!("serialize {CAPS_FILE}: {e}")))?;
    std::fs::write(out_dir.join(CAPS_FILE), text + "\n")
        .map_err(|e| Failure::Environment(format!("write {CAPS_FILE}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cap_is_written_its_largest_reading() {
        let metrics = B2buaMetrics::new();
        metrics.record_cap_reading(Cap::Legs, 3, 2);
        metrics.record_cap_reading(Cap::Legs, 1, 1);
        metrics.record_cap_reading(Cap::Timers, 7, 7);
        let read = serde_json::to_value(of(&metrics)).unwrap();
        assert_eq!(read["peak"].as_object().unwrap().len(), Cap::ALL.len());
        assert_eq!(read["peak"]["legs"], 3);
        assert_eq!(read["peak"]["timers"], 7);
        assert_eq!(read["peak"]["dialogs"], 0, "a cap read nothing is 0");
        assert_eq!(read["per_hour"]["legs"], 2);
        assert!(read["per_hour"].get("timers").is_none(), "a held-at-once cap has no rate");
    }
}
