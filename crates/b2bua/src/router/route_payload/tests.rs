use std::time::Duration;

use call::{AdmitOutcome, AdmitReport, Deadline};

use super::route_result_payload;
use crate::rules::defaults::parse_route_fold;

/// A sub-second ring deadline crosses the route payload to the fold that dials
/// the leg at its stated milliseconds.
#[test]
fn a_sub_second_ring_survives_the_route_payload() {
    let mut route = crate::decision::test_adapter::route_to("127.0.0.1", 5070);
    route.no_answer_ms = Some(800);
    let admitted = AdmitReport {
        key: "call-1#k".into(),
        change: 1,
        entries: Vec::new(),
        outcome: AdmitOutcome::NotSent,
    };
    let payload = route_result_payload(route, admitted, None);
    assert_eq!(payload["no_answer_ms"], 800);
    let fold = parse_route_fold(&payload).expect("a well-formed fold");
    assert_eq!(fold.no_answer, Deadline::After(Duration::from_millis(800)));
}
