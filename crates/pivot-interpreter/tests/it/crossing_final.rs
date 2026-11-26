//! RFC 3261 §12.3, §17.2.1 at the run level: the callee leg rings, then
//! refuses the call 487, and the caller sends an INFO on the early dialog.
//! Sent before the 487 reached the caller, the INFO crosses the final: the
//! leg answers it 481 and the run fails nothing. Sent after the caller ACKed
//! the 487, it names a dialog the caller knew ended: 481 and a failure.
//!
//! The system side is a scripted agent playing the caller; the document holds
//! the callee's leg alone. The caller's ACK of the 487 leaves when its
//! `expect(487)` reads the final, so the order of the INFO and that expect is
//! the order of the INFO and the ACK on the wire.

use std::sync::atomic::Ordering;
use std::time::Duration;

use pivot_interpreter::{Failure, VerdictStatus};
use pivot_schema::bundle::Arrived;
use pivot_schema::PivotV3;
use scenario_harness::{ClientInvite, Harness, OFFER_SDP};
use sip_message::generators::InDialogMethod;

use super::foreign_dialog::{refusal, run, ScriptedSystem};

/// The callee's leg: the INVITE, its 180, its 487 100 ms later, the ACK.
fn callee_document(id: &str) -> PivotV3 {
    let delay = |from: &str, ms: u64| serde_json::json!({ "compressible": false, "from": from, "ms": ms, "timer_linked": false });
    let doc = serde_json::json!({
        "pivot_version": 3,
        "actors": [{ "endpoint": "ep-callee", "id": "uas1", "kind": "uas",
                     "claim": { "by": "arrival-order" } }],
        "calls": [{ "attempts": [], "caller_leg": "B", "id": "c1" }],
        "case": { "family": "transparent", "id": id, "lanes": { "upstream-demo": "ok" },
                  "origin": "authored", "title": id, "variant": "repro" },
        "endpoints": [{ "binding": "dedicated", "id": "ep-callee",
                        "observed": "127.0.0.1:5070", "side": "peer" }],
        "identities": [{ "forms": ["private"], "kind": "external-caller", "name": "caller" }],
        "legs": [{ "actor": "uas1", "dir": "in", "id": "B" }],
        "timing": { "expect_budget_ms": 10000, "settle_budget_ms": 10000 },
        "flow": [
            { "id": "s1", "leg": "B", "op": "expect", "check": "record", "delay": delay("trigger", 0),
              "msg": { "method": "INVITE" } },
            { "id": "s2", "leg": "B", "op": "send", "delay": delay("step:s1", 0),
              "msg": { "cseq-method": "INVITE", "reason": "Ringing", "status": 180 } },
            { "id": "s3", "leg": "B", "op": "send", "delay": delay("step:s2", 100),
              "msg": { "cseq-method": "INVITE", "reason": "Request Terminated", "status": 487 } },
            { "id": "s4", "leg": "B", "op": "expect", "auto": true, "check": "record",
              "delay": delay("step:s3", 0), "msg": { "cseq": 1, "method": "ACK" } }
        ]
    });
    PivotV3::from_json(&doc.to_string()).expect("the document parses")
}

/// The caller's INFO on the early dialog, answered 481.
async fn info_refused(call: &mut ClientInvite) {
    let mut info = call.send_request(InDialogMethod::Info).send().await;
    info.expect(481).await;
}

/// The failures the refused INFO drew.
fn info_failures(outcome: &pivot_interpreter::Outcome) -> usize {
    outcome
        .verdict
        .failures
        .iter()
        .filter(|f| {
            matches!(f, Failure::UnexpectedDatagram { arrived: Arrived::Request { method: m, .. }, .. }
                if m == "INFO")
        })
        .count()
}

/// The caller sends the INFO once the 487 reached its socket, before it read
/// the final and ACKed it: the INFO crossed the 487, and the leg refuses it
/// 481 with no failure.
#[tokio::test(start_paused = true)]
async fn an_info_crossing_the_callees_487_is_refused_481_and_fails_nothing() {
    let h = Harness::new("pivot-crossing-final");
    let caller = h.agent("caller", "127.0.0.1:5162").await;
    let callee = h.agent("callee", "127.0.0.1:5070").await;
    let sut = ScriptedSystem::default();
    let system = async {
        let mut call = caller.invite(&callee).with_sdp(OFFER_SDP).send().await;
        call.expect(180).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        info_refused(&mut call).await;
        call.expect(487).await;
        sut.done.store(true, Ordering::SeqCst);
    };
    let document = callee_document("crossing-final");
    let run = run(document, ("uas1", &callee), caller.addr(), &sut, "crossing-final");
    let (outcome, ()) = tokio::join!(run, system);

    let (_, answer) = refusal(&outcome, "B", "INFO");
    assert!(answer.note.as_deref().is_some_and(|n| n.starts_with("481")), "{answer:#?}");
    assert_eq!(info_failures(&outcome), 0, "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.status, VerdictStatus::Ok, "{:#?}", outcome.verdict.failures);
    let _ = h.finish().await;
}

/// The caller ACKs the 487, then sends the INFO: the leg refuses it 481 and
/// fails the run on it.
#[tokio::test(start_paused = true)]
async fn an_info_after_the_ack_of_the_callees_487_is_refused_481_and_fails() {
    let h = Harness::new("pivot-after-final-ack");
    let caller = h.agent("caller", "127.0.0.1:5163").await;
    let callee = h.agent("callee", "127.0.0.1:5071").await;
    let sut = ScriptedSystem::default();
    let system = async {
        let mut call = caller.invite(&callee).with_sdp(OFFER_SDP).send().await;
        call.expect(180).await;
        call.expect(487).await;
        info_refused(&mut call).await;
        sut.done.store(true, Ordering::SeqCst);
    };
    let document = callee_document("after-final-ack");
    let run = run(document, ("uas1", &callee), caller.addr(), &sut, "after-final-ack");
    let (outcome, ()) = tokio::join!(run, system);

    let (_, answer) = refusal(&outcome, "B", "INFO");
    assert!(answer.note.as_deref().is_some_and(|n| n.starts_with("481")), "{answer:#?}");
    assert_eq!(info_failures(&outcome), 1, "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    let _ = h.finish().await;
}
