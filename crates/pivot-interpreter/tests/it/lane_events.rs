//! A send that awaits a lane event (`PCAP2TEST_PIVOT_V3.md` §6.7g): the caller's
//! CANCEL goes out only once the lane reports that the system has a request to
//! its decision backend in flight, and a run whose lane cannot report the event
//! refuses before it dials.
//!
//! The system side is a scripted agent: it rings, then answers the CANCEL and
//! closes the INVITE `487` (RFC 3261 §9.2). The document holds the caller's leg
//! alone.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pivot_interpreter::{
    Booking, ClockMode, Failure, IdentityBindings, Lane, LaneEvents, Outcome, RunConfig, Sut,
    UriComposer, VerdictStatus,
};
use pivot_schema::PivotV3;
use scenario_harness::{Agent, Harness};
use tokio::time::Instant;

const EVENT: &str = "decision-request-in-flight";

/// How long after its 180 the lane reports the event.
const HOLD: Duration = Duration::from_millis(700);

/// The caller's own number on this lane; every other address is frozen.
struct Composer;

impl UriComposer for Composer {
    fn compose(&self, pos: &str, _form: Option<&str>, _is_ruri: bool) -> Option<String> {
        (pos == "caller").then(|| "sip:0009001@pivot.invalid".to_string())
    }

    fn frozen(&self, value: &str, _kind: Option<&str>, _is_ruri: bool) -> String {
        value.to_string()
    }
}

/// A system with no CDR oracle whose call stays active until its script ends.
#[derive(Default)]
struct ScriptedSystem {
    done: AtomicBool,
}

impl Sut for ScriptedSystem {
    fn active_calls(&self) -> usize {
        usize::from(!self.done.load(Ordering::SeqCst))
    }

    fn cdr_records(&self) -> Vec<BTreeMap<String, String>> {
        Vec::new()
    }

    fn observe(&self, name: &str) -> Result<Option<String>, String> {
        Err(format!("{name:?} is not an observable this lane publishes"))
    }
}

/// The caller's leg: INVITE, the 180, then a CANCEL awaiting the event, its
/// 200 and the INVITE's 487.
fn caller_document(id: &str, sys: SocketAddr, cancel_within_ms: Option<u64>) -> PivotV3 {
    awaiting_document(id, sys, cancel_within_ms, false)
}

/// [`caller_document`], the INVITE awaiting the same event where `invite_awaits`.
fn awaiting_document(
    id: &str,
    sys: SocketAddr,
    cancel_within_ms: Option<u64>,
    invite_awaits: bool,
) -> PivotV3 {
    let delay = |from: &str| serde_json::json!({ "compressible": true, "from": from, "ms": 0, "timer_linked": false });
    let mut cancel = serde_json::json!({
        "id": "s3", "leg": "A", "op": "send", "awaits": EVENT, "delay": delay("step:s2"),
        "msg": { "method": "CANCEL" }
    });
    if let Some(ms) = cancel_within_ms {
        cancel["within_ms"] = ms.into();
    }
    let mut doc = serde_json::json!({
        "pivot_version": 3,
        "actors": [{ "endpoint": "ep-caller", "id": "uac1", "identity": "caller", "kind": "uac" }],
        "calls": [{ "attempts": [], "caller_leg": "A", "id": "c1" }],
        "case": { "family": "transparent", "id": id, "lanes": { "upstream-demo": "ok" },
                  "origin": "authored", "title": id, "variant": "repro" },
        "endpoints": [{ "binding": "dedicated", "id": "ep-caller",
                        "observed": "127.0.0.1:5060", "side": "peer" }],
        "identities": [{ "forms": ["private"], "kind": "external-caller", "name": "caller" }],
        "legs": [{ "actor": "uac1", "dir": "out", "id": "A" }],
        "timing": { "expect_budget_ms": 10000, "settle_budget_ms": 10000 },
        "flow": [
            { "id": "s1", "leg": "A", "op": "send", "delay": delay("trigger"),
              "msg": { "method": "INVITE",
                       "body": { "ref": "resources/uac1_offer.sdp", "rewrite": ["c=addr", "m=port"] },
                       "ruri": { "frozen": format!("sip:callee@{sys}") },
                       "from": { "form": "private", "pos": "caller" },
                       "to": { "frozen": "sip:callee@pivot.invalid" } } },
            { "id": "s2", "leg": "A", "op": "expect", "check": "record", "delay": delay("step:s1"),
              "msg": { "cseq-method": "INVITE", "reason": "Ringing", "status": 180 } },
            cancel,
            { "id": "s4", "leg": "A", "op": "expect", "check": "record", "delay": delay("step:s3"),
              "msg": { "cseq-method": "CANCEL", "reason": "OK", "status": 200 } },
            { "id": "s5", "leg": "A", "op": "expect", "check": "record", "delay": delay("step:s3"),
              "msg": { "cseq-method": "INVITE", "reason": "Request Terminated", "status": 487 } }
        ]
    });
    if invite_awaits {
        doc["flow"][0]["awaits"] = EVENT.into();
    }
    PivotV3::from_json(&doc.to_string()).expect("the document parses")
}

/// Run `document` with `alice` as the caller, the lane reporting `events`.
async fn run(
    document: PivotV3,
    alice: &Agent,
    sys: SocketAddr,
    system: &ScriptedSystem,
    events: Option<LaneEvents>,
    case: &str,
) -> Outcome {
    let base_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let config = RunConfig::new("upstream-demo", ClockMode::Virtual, sys.to_string())
        .with_identities(IdentityBindings::new().bind("caller", "private", "0009001"));
    let lane = Lane {
        agents: BTreeMap::from([("uac1".to_string(), alice.clone())]),
        route_target: sys,
        media: Booking::new("127.0.0.1", 40000),
        base_dir,
        composer: &Composer,
        events,
        reader: std::sync::Arc::new(sip_message::CustomParser::built()),
    };
    let dir = std::env::temp_dir().join("pivot-runs").join(case);
    pivot_interpreter::replay(document, config, lane, system, &dir)
        .await
        .expect("the run replays and leaves its bundle")
}

#[tokio::test(start_paused = true)]
async fn a_send_awaiting_a_lane_event_goes_out_once_the_lane_reports_it() {
    let h = Harness::new("pivot-lane-event-released");
    let alice = h.agent("alice", "127.0.0.1:5162").await;
    let sys = h.agent("sys", "127.0.0.1:5197").await;
    let document = caller_document("lane-event-released", sys.addr(), None);
    let sut = ScriptedSystem::default();
    let (reporter, events) = LaneEvents::channel([EVENT]);
    let system = async {
        let mut uas = sys.receive("INVITE").await;
        uas.respond(180, "Ringing").await;
        let rang = Instant::now();
        let report = async {
            tokio::time::sleep(HOLD).await;
            reporter.occurred(EVENT);
        };
        let arrival = async {
            let cancel = sys.receive("CANCEL").await;
            (cancel, Instant::now() - rang)
        };
        let ((mut cancel, waited), ()) = tokio::join!(arrival, report);
        cancel.respond(200, "OK").await;
        uas.respond(487, "Request Terminated").await;
        uas.expect_ack().await;
        sut.done.store(true, Ordering::SeqCst);
        waited
    };
    let (outcome, waited) =
        tokio::join!(run(document, &alice, sys.addr(), &sut, Some(events), "released"), system);

    assert_eq!(outcome.verdict.status, VerdictStatus::Ok, "{:#?}", outcome.verdict.failures);
    assert!(waited >= HOLD, "the CANCEL waited for the event: it arrived {waited:?} after the 180");
    let _ = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_send_whose_lane_event_never_occurs_fails_at_its_budget() {
    let h = Harness::new("pivot-lane-event-missing");
    let alice = h.agent("alice", "127.0.0.1:5163").await;
    let sys = h.agent("sys", "127.0.0.1:5196").await;
    let document = caller_document("lane-event-missing", sys.addr(), Some(2000));
    let sut = ScriptedSystem::default();
    let (_reporter, events) = LaneEvents::channel([EVENT]);
    let system = async {
        let mut uas = sys.receive("INVITE").await;
        uas.respond(180, "Ringing").await;
        // The script ends at the missing event and the generic close CANCELs.
        let mut cancel = sys.receive("CANCEL").await;
        cancel.respond(200, "OK").await;
        uas.respond(487, "Request Terminated").await;
        uas.expect_ack().await;
        sut.done.store(true, Ordering::SeqCst);
    };
    let (outcome, ()) =
        tokio::join!(run(document, &alice, sys.addr(), &sut, Some(events), "missing"), system);

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert!(
        outcome.verdict.failures.iter().any(|f| matches!(f,
            Failure::LaneEventMissing { step, event, within_ms: 2000, .. }
                if step == "s3" && event == EVENT)),
        "{:#?}",
        outcome.verdict.failures
    );
    let _ = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_lane_that_does_not_report_the_event_refuses_before_it_dials() {
    let h = Harness::new("pivot-lane-event-unsupported");
    let alice = h.agent("alice", "127.0.0.1:5164").await;
    let sys = h.agent("sys", "127.0.0.1:5195").await;
    let document = caller_document("lane-event-unsupported", sys.addr(), None);
    let sut = ScriptedSystem { done: AtomicBool::new(true) };
    let outcome = run(document, &alice, sys.addr(), &sut, None, "unsupported").await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert!(
        outcome.verdict.failures.iter().any(|f| matches!(f,
            Failure::LaneEventUnsupported { step, event } if step == "s3" && event == EVENT)),
        "{:#?}",
        outcome.verdict.failures
    );
    assert!(outcome.recording.legs().get("A").is_none_or(Vec::is_empty), "nothing was dialled");
    let _ = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn one_occurrence_releases_exactly_one_of_two_awaiting_sends() {
    let h = Harness::new("pivot-lane-event-one-release");
    let alice = h.agent("alice", "127.0.0.1:5165").await;
    let sys = h.agent("sys", "127.0.0.1:5194").await;
    let document = awaiting_document("lane-event-one-release", sys.addr(), Some(2000), true);
    let sut = ScriptedSystem::default();
    let (reporter, events) = LaneEvents::channel([EVENT]);
    reporter.occurred(EVENT);
    let system = async {
        let mut uas = sys.receive("INVITE").await;
        uas.respond(180, "Ringing").await;
        // The INVITE took the one occurrence; the generic close CANCELs.
        let mut cancel = sys.receive("CANCEL").await;
        cancel.respond(200, "OK").await;
        uas.respond(487, "Request Terminated").await;
        uas.expect_ack().await;
        sut.done.store(true, Ordering::SeqCst);
    };
    let (outcome, ()) =
        tokio::join!(run(document, &alice, sys.addr(), &sut, Some(events), "one-release"), system);

    let missing: Vec<&str> = outcome
        .verdict
        .failures
        .iter()
        .filter_map(|f| match f {
            Failure::LaneEventMissing { step, .. } => Some(step.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(missing, ["s3"], "{:#?}", outcome.verdict.failures);
    let _ = h.finish().await;
}
