//! Recorder + severity + sequencer foundations.

use layer_harness::{
    lane_key, NetworkTag, RecordedAnomaly, Recorder, RunContext, Severity,
};

#[derive(Clone, PartialEq, Eq, Debug)]
struct DemoEvent {
    n: u32,
}

#[test]
fn typed_channel_records_and_stamps_in_order() {
    let recorder = Recorder::fake();
    let ch = recorder.for_tag::<DemoEvent>("demo");
    ch.record(DemoEvent { n: 1 });
    ch.record(DemoEvent { n: 2 });

    let snap = ch.snapshot();
    assert_eq!(snap.len(), 2);
    assert_eq!(snap[0].event.n, 1);
    assert_eq!(snap[1].event.n, 2);
    // Shared sequencer is strictly increasing.
    assert!(snap[1].seq > snap[0].seq);
}

#[test]
fn for_tag_reopens_the_same_buffer() {
    let recorder = Recorder::fake();
    recorder.for_tag::<DemoEvent>("demo").record(DemoEvent { n: 9 });
    // Re-opening the same key sees the earlier record.
    let again = recorder.for_tag::<DemoEvent>("demo");
    assert_eq!(again.snapshot().len(), 1);
}

#[test]
fn projector_findings_merge_into_snapshot() {
    let recorder = Recorder::fake();
    let seq = recorder.sequencer();
    recorder.register_projector("demo", move || {
        vec![RecordedAnomaly::new(
            "demoFinding",
            "P1",
            "projected",
            Severity::Advisory,
            None,
            seq.next(),
            0,
        )]
    });
    let snap = recorder.snapshot();
    assert!(snap.anomalies.iter().any(|a| a.kind == "demoFinding"));
}

#[test]
fn lane_name_conflict_is_recorded_once() {
    let recorder = Recorder::fake();
    let addr = "10.0.0.1:5060".parse().unwrap();
    recorder.register_lane(addr, "alice", NetworkTag::Ext);
    recorder.register_lane(addr, "alice", NetworkTag::Ext); // idempotent
    recorder.register_lane(addr, "bob", NetworkTag::Ext); // conflict
    recorder.register_lane(addr, "carol", NetworkTag::Ext); // still one anomaly

    let conflicts = recorder
        .anomalies()
        .into_iter()
        .filter(|a| a.kind == "nameConflict")
        .count();
    assert_eq!(conflicts, 1);

    let snap = recorder.snapshot();
    let lane = snap.lanes.iter().find(|l| lane_key(l.addr) == lane_key(addr)).unwrap();
    assert_eq!(lane.names, vec!["alice", "bob", "carol"]);
}

#[test]
fn severity_tiers_route_by_context() {
    const TAG: &str = "demo-layer";

    // real-run: never fails.
    assert_eq!(
        RunContext::RealRun.severity_for(TAG, false),
        Severity::Advisory
    );
    assert!(!RunContext::RealRun.rules_enabled());

    // recorder mode: deferred-fail, unless the rule forces advisory.
    assert_eq!(
        RunContext::TestWithRecorder.severity_for(TAG, false),
        Severity::DeferredFail
    );
    assert_eq!(
        RunContext::TestWithRecorder.severity_for(TAG, true),
        Severity::Advisory
    );

    // unit-test-of-layer: fatal for the targeted tag, advisory for others.
    assert_eq!(
        RunContext::UnitTestOfLayer { tag: TAG }.severity_for(TAG, false),
        Severity::Fatal
    );
    assert_eq!(
        RunContext::UnitTestOfLayer { tag: "other" }.severity_for(TAG, false),
        Severity::Advisory
    );
}
