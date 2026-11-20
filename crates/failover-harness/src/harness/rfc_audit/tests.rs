//! The report's RFC anomalies.

use std::collections::BTreeMap;

use super::rfc_anomalies;
use crate::rfc_acceptance::Partitioned;

/// A finding informative inside the system under test is listed, advisory,
/// and does not fail the run; a gating one does.
#[test]
fn an_informative_finding_is_listed_advisory_and_never_fails_the_run() {
    let finding = |lane: &str| ("rule-x".to_string(), lane.to_string(), "detail".to_string());
    let informative = Partitioned { informative: vec![finding("proxy")], ..Default::default() };
    let (gates, anomalies) = rfc_anomalies(informative, &BTreeMap::new());
    assert!(!gates);
    assert_eq!(anomalies.len(), 1, "{anomalies:?}");
    assert_eq!(anomalies[0].advisory, Some(true));
    assert!(anomalies[0].detail.starts_with("INFORMATIVE inside the system under test"));

    let gating = Partitioned { gating: vec![finding("proxy")], ..Default::default() };
    let (gates, anomalies) = rfc_anomalies(gating, &BTreeMap::new());
    assert!(gates);
    assert_eq!(anomalies[0].advisory, Some(false));
}
