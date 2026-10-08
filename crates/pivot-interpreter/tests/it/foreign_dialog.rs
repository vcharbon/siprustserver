//! RFC 3261 §12.2.2 at the run level: a request taken on a confirmed leg under
//! a From-tag naming no dialog the leg holds is answered `481`, recorded, and
//! failed — mid-flow and during settle alike.
//!
//! The system side is a scripted agent playing a forking callee behind a
//! transparent relay: it rings under fork `f1`, answers under `f2`, which never
//! rang, and later sends the caller a request under `f1` past the 64·T1 the
//! early dialog of `f1` stood (§13.2.2.4). The document holds the caller's leg
//! alone.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pivot_interpreter::{
    Booking, ClockMode, Failure, IdentityBindings, Lane, Outcome, RunConfig, Sut, UriComposer,
    VerdictStatus,
};
use pivot_schema::bundle::recording::{Dir, RecordedMessage};
use pivot_schema::bundle::Arrived;
use pivot_schema::PivotV3;
use scenario_harness::{Agent, Harness, WaiverScope};
use sip_message::generators::InDialogMethod;

const ANSWER: &str = "v=0\r\no=sys 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// Past the 64·T1 (32 s) an early dialog of another fork stands after the 2xx.
const PAST_THE_WINDOW: Duration = Duration::from_secs(33);

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

/// The caller's leg: INVITE, the 180 and the 2xx it draws, the ACK, then its
/// BYE `bye_after_ms` after the ACK.
fn caller_document(id: &str, sys: SocketAddr, bye_after_ms: u64) -> PivotV3 {
    let delay = |from: &str, ms: u64| serde_json::json!({ "compressible": false, "from": from, "ms": ms, "timer_linked": false });
    let doc = serde_json::json!({
        "pivot_version": 3,
        "actors": [{ "endpoint": "ep-caller", "id": "uac1", "identity": "caller", "kind": "uac" }],
        "calls": [{ "attempts": [], "caller_leg": "A", "id": "c1" }],
        "case": { "family": "transparent", "id": id, "lanes": { "upstream-demo": "ok" },
                  "origin": "authored", "title": id, "variant": "repro" },
        "endpoints": [{ "binding": "dedicated", "id": "ep-caller",
                        "observed": "127.0.0.1:5060", "side": "peer" }],
        "identities": [{ "forms": ["private"], "kind": "external-caller", "name": "caller" }],
        "legs": [{ "actor": "uac1", "dir": "out", "id": "A" }],
        "timing": { "expect_budget_ms": 60000, "settle_budget_ms": 10000 },
        "flow": [
            { "id": "s1", "leg": "A", "op": "send", "delay": delay("trigger", 0),
              "msg": { "method": "INVITE",
                       "body": { "ref": "resources/uac1_offer.sdp", "rewrite": ["c=addr", "m=port"] },
                       "ruri": { "frozen": format!("sip:callee@{sys}") },
                       "from": { "form": "private", "pos": "caller" },
                       "to": { "frozen": "sip:callee@pivot.invalid" } } },
            { "id": "s2", "leg": "A", "op": "expect", "check": "record", "delay": delay("step:s1", 0),
              "msg": { "cseq-method": "INVITE", "reason": "Ringing", "status": 180 } },
            { "id": "s3", "leg": "A", "op": "expect", "check": "record", "delay": delay("step:s2", 0),
              "msg": { "cseq-method": "INVITE", "reason": "OK", "status": 200 } },
            { "id": "s4", "leg": "A", "op": "send", "auto": true, "confirms_dialog": true,
              "in_dialog": true, "delay": delay("step:s3", 0), "msg": { "cseq": 1, "method": "ACK" } },
            { "id": "s5", "leg": "A", "op": "send", "in_dialog": true,
              "delay": delay("step:s4", bye_after_ms), "msg": { "method": "BYE" } },
            { "id": "s6", "leg": "A", "op": "expect", "check": "record", "in_dialog": true,
              "delay": delay("step:s5", 0),
              "msg": { "cseq-method": "BYE", "reason": "OK", "status": 200 } }
        ]
    });
    PivotV3::from_json(&doc.to_string()).expect("the document parses")
}

/// Run `document` with `alice` as the caller against the scripted system.
async fn run(
    document: PivotV3,
    alice: &Agent,
    sys: SocketAddr,
    system: &ScriptedSystem,
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
    };
    let dir = std::env::temp_dir().join("pivot-runs").join(case);
    pivot_interpreter::replay(document, config, lane, system, &dir)
        .await
        .expect("the run replays and leaves its bundle")
}

/// The system rings under `f1`, answers under `f2` and takes the ACK; the
/// returned transaction mints requests under `f1` (`dialog()` after
/// `adopt_to_tag`).
async fn ring_f1_answer_f2(sys: &Agent) -> scenario_harness::ServerTxn {
    let mut uas = sys.receive("INVITE").await;
    uas.respond(180, "Ringing").with_to_tag("f1").await;
    uas.respond(200, "OK").with_to_tag("f2").with_sdp(ANSWER).await;
    sys.receive("ACK").await;
    uas
}

/// The 481 the caller sent, with its recording note, and the refused request's
/// own line before it.
fn refusal(outcome: &Outcome, method: &str) -> (RecordedMessage, RecordedMessage) {
    let legs = outcome.recording.legs();
    let ladder = &legs["A"];
    let at = ladder
        .iter()
        .position(|m| {
            m.dir == Dir::Out && String::from_utf8_lossy(m.wire()).starts_with("SIP/2.0 481")
        })
        .unwrap_or_else(|| panic!("the caller answered 481: {ladder:#?}"));
    let refused = ladder[..at]
        .iter()
        .rev()
        .find(|m| m.dir == Dir::In && String::from_utf8_lossy(m.wire()).starts_with(method))
        .cloned()
        .expect("the refused request is recorded before its 481");
    (refused, ladder[at].clone())
}

fn foreign_failures(outcome: &Outcome, method: &str) -> usize {
    outcome
        .verdict
        .failures
        .iter()
        .filter(|f| {
            matches!(f, Failure::UnexpectedDatagram { arrived: Arrived::Request { method: m, .. },
                detail: Some(d), .. } if m == method && d.contains("names no dialog"))
        })
        .count()
}

/// Mid-flow: the system's OPTIONS under `f1`, past the window, while the
/// caller's BYE is still a dwell away.
#[tokio::test(start_paused = true)]
async fn a_request_under_an_abandoned_fork_tag_is_refused_481_mid_flow() {
    let h = Harness::new("pivot-foreign-dialog-mid-flow");
    h.waive(
        WaiverScope::rule(
            "in-dialog-from-tag",
            "the system's request under f1 is the deviation under test",
        )
        .on_party("sys"),
    );
    let alice = h.agent("alice", "127.0.0.1:5160").await;
    let sys = h.agent("sys", "127.0.0.1:5199").await;
    let document = caller_document("foreign-dialog-mid-flow", sys.addr(), 35_000);
    let sut = ScriptedSystem::default();
    let system = async {
        let mut uas = ring_f1_answer_f2(&sys).await;
        tokio::time::sleep(PAST_THE_WINDOW).await;
        uas.adopt_to_tag("f1");
        let mut stray = uas.dialog().request(InDialogMethod::Options, None).await;
        stray.expect(481).await;
        let mut bye = sys.receive("BYE").await;
        assert_eq!(bye.request().to().tag(), Some("f2"), "the caller's BYE names f2");
        bye.respond(200, "OK").await;
        sut.done.store(true, Ordering::SeqCst);
    };
    let (outcome, ()) = tokio::join!(run(document, &alice, sys.addr(), &sut, "mid-flow"), system);

    let (refused, answer) = refusal(&outcome, "OPTIONS");
    assert!(refused.note.as_deref().is_some_and(|n| n.contains("names no dialog")), "{refused:#?}");
    assert!(answer.note.as_deref().is_some_and(|n| n.starts_with("481")), "{answer:#?}");
    assert_eq!(foreign_failures(&outcome, "OPTIONS"), 1, "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    let _ = h.finish().await;
}

/// During settle: the flow completed with the caller's BYE, then the system
/// sends an OPTIONS under `f1` past the window.
#[tokio::test(start_paused = true)]
async fn a_request_under_an_abandoned_fork_tag_is_refused_481_during_settle() {
    let h = Harness::new("pivot-foreign-dialog-settle");
    h.waive(
        WaiverScope::rule(
            "in-dialog-from-tag",
            "the system's request under f1 is the deviation under test",
        )
        .on_party("sys"),
    );
    let alice = h.agent("alice", "127.0.0.1:5161").await;
    let sys = h.agent("sys", "127.0.0.1:5198").await;
    let document = caller_document("foreign-dialog-settle", sys.addr(), 33_000);
    let sut = ScriptedSystem::default();
    let system = async {
        let mut uas = ring_f1_answer_f2(&sys).await;
        tokio::time::sleep(Duration::from_secs(32)).await;
        sys.receive("BYE").await.respond(200, "OK").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        uas.adopt_to_tag("f1");
        let mut stray = uas.dialog().request(InDialogMethod::Options, None).await;
        stray.expect(481).await;
        sut.done.store(true, Ordering::SeqCst);
    };
    let (outcome, ()) = tokio::join!(run(document, &alice, sys.addr(), &sut, "settle"), system);

    let (refused, answer) = refusal(&outcome, "OPTIONS");
    assert!(refused.note.as_deref().is_some_and(|n| n.contains("names no dialog")), "{refused:#?}");
    assert!(answer.note.as_deref().is_some_and(|n| n.starts_with("481")), "{answer:#?}");
    assert_eq!(foreign_failures(&outcome, "OPTIONS"), 1, "{:#?}", outcome.verdict.failures);
    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    let _ = h.finish().await;
}
