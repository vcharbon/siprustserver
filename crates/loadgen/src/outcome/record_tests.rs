use scenario_harness::actor::FailedAt;
use serde_json::json;

use super::*;
use crate::outcome::wire::{LegFacts, WireFailure};

const META: RecordMeta<'static> =
    RecordMeta { run: "r1", source: "loadgen", stream: "main", shape: "basic_call" };

fn placed() -> WireFacts {
    WireFacts {
        call_id: Some("c1".into()),
        started_at_ms: Some(1_000),
        first_provisional_at_ms: Some(1_010),
        ..WireFacts::default()
    }
}

fn answered() -> WireFacts {
    WireFacts {
        setup_ended_at_ms: Some(1_500),
        answered_at_ms: Some(1_500),
        final_code: Some(200),
        ..placed()
    }
}

fn rejected(code: u16, cause: Option<&str>) -> WireFacts {
    WireFacts {
        setup_ended_at_ms: Some(1_200),
        final_code: Some(code),
        final_cause: cause.map(str::to_string),
        ..placed()
    }
}

fn ok() -> CallOutcome {
    CallOutcome::Ok { branch: "answered" }
}

fn step(cause: StepError) -> CallOutcome {
    CallOutcome::Step { at: FailedAt::at_barrier("established"), cause }
}

fn timeout() -> CallOutcome {
    step(StepError::timeout("alice", None, "a datagram"))
}

fn record(wire: &WireFacts, outcome: &CallOutcome) -> Value {
    build(&META, wire, outcome, 9_000).expect("a placed call has a record")
}

/// An answered call torn down by its BYE: `answered`, released, no reason.
#[test]
#[ignore = "slow lane: loadgen"]
fn an_answered_call_reads_answered_and_released() {
    let wire = WireFacts { bye: Some(Some(200)), ..answered() };
    let r = record(&wire, &ok());
    assert_eq!(r["schema"], "call-outcome.v1");
    assert_eq!(
        (r["ending"].clone(), r["phase_reached"].clone()),
        (json!("answered"), json!("released"))
    );
    assert_eq!((r["final_code"].clone(), r["reason"].clone()), (json!(200), Value::Null));
    assert_eq!(r["producer_verdict"], "ok:answered");
    assert_eq!(
        (r["run"].clone(), r["stream"].clone(), r["shape"].clone()),
        (json!("r1"), json!("main"), json!("basic_call"))
    );
    assert_eq!(r["first_provisional_at_ms"], 1_010);
    assert_eq!(r["ended_at_ms"], 9_000);
}

/// A non-2xx final is `rejected` whatever loadgen made of it: the reason is
/// the Reason header's cause key, else the code; a CANCELed call's 487 is
/// `abandoned`.
#[test]
#[ignore = "slow lane: loadgen"]
fn a_non_2xx_final_is_rejected_or_abandoned() {
    let r = record(&rejected(486, None), &ok());
    assert_eq!((r["ending"].clone(), r["reason"].clone()), (json!("rejected"), json!("486")));
    let wrong = step(StepError::WrongStatus {
        who: "alice".into(),
        expected: 200,
        got: 503,
        reason: "x".into(),
    });
    let r = record(&rejected(503, Some("SIP;cause=503")), &wrong);
    assert_eq!(
        (r["ending"].clone(), r["reason"].clone()),
        (json!("rejected"), json!("SIP;cause=503"))
    );
    assert_eq!(r["producer_verdict"], "status_503");
    assert_eq!(r["phase_reached"], "provisional");
    let r = record(&WireFacts { cancelled: true, ..rejected(487, None) }, &ok());
    assert_eq!(r["ending"], "abandoned");
}

/// No final: a wait that passed is `timeout` at the record's end; a transport
/// error `failed`/`transport`; a panic `failed` with a generator failure.
#[test]
#[ignore = "slow lane: loadgen"]
fn without_a_final_the_result_names_the_ending() {
    let r = record(&placed(), &timeout());
    assert_eq!((r["ending"].clone(), r["reason"].clone()), (json!("timeout"), json!("timeout")));
    assert_eq!(r["setup_ended_at_ms"], 9_000);
    assert_eq!(r["final_code"], Value::Null);
    let r = record(
        &placed(),
        &step(StepError::Transport { who: "alice".into(), detail: "refused".into() }),
    );
    assert_eq!((r["ending"].clone(), r["reason"].clone()), (json!("failed"), json!("transport")));
    assert_eq!(r["failures"][0]["kind"], "transport");
    let r = record(&placed(), &CallOutcome::Panic("boom".into()));
    assert_eq!(r["ending"], "failed");
    assert_eq!(r["failures"][0]["kind"], "generator");
    assert_eq!(r["producer_verdict"], "panic");
}

/// After answer: a 481 cuts the call, a BYE with no final strands it, and an
/// in-dialog request's non-2xx final is a `dialog_request` failure.
#[test]
#[ignore = "slow lane: loadgen"]
fn after_answer_the_dialog_facts_name_the_ending() {
    let failed = WireFailure { at_ms: 2_000, method: "INVITE".into(), code: 481 };
    let cut =
        WireFacts { saw_481: true, failures: vec![failed], bye: Some(Some(200)), ..answered() };
    let wrong = step(StepError::WrongStatus {
        who: "alice".into(),
        expected: 200,
        got: 481,
        reason: "x".into(),
    });
    let r = record(&cut, &wrong);
    assert_eq!(r["ending"], "cut");
    assert_eq!(
        r["failures"][0],
        json!({ "at_ms": 2_000, "kind": "dialog_request", "method": "INVITE", "code": 481 })
    );
    let r = record(&WireFacts { bye: Some(None), ..answered() }, &timeout());
    assert_eq!(
        (r["ending"].clone(), r["phase_reached"].clone()),
        (json!("stranded"), json!("answered"))
    );
    let r = record(&WireFacts { bye_received: Some(Some(200)), ..answered() }, &timeout());
    assert_eq!(r["ending"], "cut", "a BYE the caller did not wait for");
}

/// Each callee leg reads its own ending.
#[test]
#[ignore = "slow lane: loadgen"]
fn each_leg_reads_its_ending() {
    let legs = vec![
        LegFacts { role: "primary".into(), final_code: Some(486), ..LegFacts::default() },
        LegFacts {
            role: "alt".into(),
            final_code: Some(200),
            early_media: true,
            ..LegFacts::default()
        },
        LegFacts {
            role: "late".into(),
            final_code: Some(487),
            cancelled: true,
            ..LegFacts::default()
        },
        LegFacts { role: "mute".into(), ..LegFacts::default() },
    ];
    let r = record(&WireFacts { legs, bye: Some(Some(200)), ..answered() }, &ok());
    let endings: Vec<&str> =
        r["legs"].as_array().unwrap().iter().map(|l| l["ending"].as_str().unwrap()).collect();
    assert_eq!(endings, ["rejected", "answered", "abandoned", "timeout"]);
    assert_eq!(r["legs"][1]["early_media"], true);
}

/// A call refused before any datagram, or one that sent no INVITE, has no
/// record.
#[test]
#[ignore = "slow lane: loadgen"]
fn a_call_never_placed_has_no_record() {
    assert!(build(&META, &placed(), &CallOutcome::Rejected("key_in_flight"), 1).is_none());
    assert!(build(&META, &WireFacts::default(), &CallOutcome::Panic("p".into()), 1).is_none());
}
