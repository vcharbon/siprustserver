//! The lane's injector (§6.6): an `inject` node is handed over once its `after`
//! and `delay` hold, the call stays up while the lane performs it, and what is
//! ordered `after` it waits for the answer. A refusal or a silence fails the
//! run by name, and the close still ends the call.

use std::sync::Mutex;
use std::time::Duration;

use pivot_interpreter::{InjectRequest, Injection, Injector};
use pivot_schema::flow::{Anchor, Delay, FlowNode, Inject, InjectOp};

use super::*;

/// An injector answering every request `after` it was handed, with `answer`,
/// or never where `answer` is `None`; it keeps what it was handed.
struct Scripted {
    after: Duration,
    answer: Option<Result<(), String>>,
    within: Duration,
    handed: Arc<Mutex<Vec<InjectRequest>>>,
}

impl Scripted {
    fn new(after_ms: u64, answer: Option<Result<(), String>>, within_ms: u64) -> Scripted {
        Scripted {
            after: Duration::from_millis(after_ms),
            answer,
            within: Duration::from_millis(within_ms),
            handed: Arc::default(),
        }
    }
}

impl Injector for Scripted {
    fn inject(&self, request: InjectRequest) -> Injection {
        self.handed.lock().unwrap().push(request);
        let (after, answer) = (self.after, self.answer.clone());
        Box::pin(async move {
            tokio::time::sleep(after).await;
            match answer {
                Some(answer) => answer,
                None => std::future::pending().await,
            }
        })
    }

    fn within(&self) -> Duration {
        self.within
    }
}

/// The linear attempt with an `inject` of `node-kill` once the callee took the
/// ACK (`s9`), the caller's BYE (`s10`) ordered after it.
fn injected_case(id: &str, delay: Option<Delay>, target: Option<&str>) -> Case {
    let mut case = fixture("linear-attempt.v3.json");
    case.document.case.id = id.into();
    let at = case.document.flow.iter().position(|node| node.id() == "s10").expect("the BYE");
    case.document.flow.insert(
        at,
        FlowNode::Inject(Inject {
            id: "i1".into(),
            op: InjectOp::Inject,
            action: "node-kill".into(),
            target: target.map(str::to_string),
            after: vec!["s9".into()],
            delay,
        }),
    );
    let FlowNode::Message(bye) = &mut case.document.flow[at + 1] else {
        panic!("s10 is a message step")
    };
    bye.after.push("i1".into());
    case
}

/// The run of `case` on the ladder's lane, `injector` performing its injects.
async fn replay_injected(
    scene: &B2buaScene,
    case: Case,
    injector: Arc<Scripted>,
) -> (Outcome, PathBuf) {
    let directives = LaneDirectives { injector: Some(injector), ..Default::default() };
    replay_directed(scene, case, BTreeMap::new(), directives).await
}

/// Microseconds from the ACK the callee took (`s9`) to the caller's BYE
/// (`s10`), off the recording.
fn ack_to_bye_us(outcome: &Outcome) -> u64 {
    let legs = outcome.recording.legs();
    let at = |step: &str| {
        legs.values()
            .flatten()
            .find(|m| m.step.as_deref() == Some(step))
            .map(|m| m.at_us)
            .unwrap_or_else(|| panic!("{step} is recorded: {legs:#?}"))
    };
    at("s10") - at("s9")
}

/// The call is billed once and the system holds nothing of it.
async fn call_ended(scene: B2buaScene) {
    assert_eq!(scene.b2bua.cdr_records().len(), 1, "the call was billed");
    b2bua_harness::settle_until(|| scene.b2bua.is_reaped()).await;
    scene.b2bua.assert_fully_reaped();
    scene.finish().await;
}

/// **An answered inject releases what waits on it**: the injector is handed
/// `i1` once, the call stays up while it works, and the BYE ordered after it
/// goes out only once it answered.
#[tokio::test(start_paused = true)]
async fn an_answered_inject_holds_the_send_after_it_until_the_answer() {
    let scene = api_scene("pivot-inject-answered").await;
    let injector = Arc::new(Scripted::new(1_500, Some(Ok(())), 10_000));
    let case = injected_case("inject-answered", None, None);
    let (outcome, dir) = replay_injected(&scene, case, injector.clone()).await;

    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(
        *injector.handed.lock().unwrap(),
        [InjectRequest { node: "i1".into(), action: "node-kill".into(), target: None }]
    );
    assert!(ack_to_bye_us(&outcome) >= 1_500_000, "{} µs", ack_to_bye_us(&outcome));
    call_ended(scene).await;
}

/// **An inject's dwell and target are the injector's to see**: `i1` is handed
/// over `delay.ms` after its anchor, its target with it.
#[tokio::test(start_paused = true)]
async fn an_inject_is_handed_over_after_its_dwell_with_its_target() {
    let scene = api_scene("pivot-inject-dwell").await;
    let injector = Arc::new(Scripted::new(0, Some(Ok(())), 10_000));
    let delay = Delay {
        ms: 1_000,
        from: Anchor::Step("s9".into()),
        compressible: true,
        timer_linked: false,
    };
    let case = injected_case("inject-dwell", Some(delay), Some("node-b"));
    let (outcome, dir) = replay_injected(&scene, case, injector.clone()).await;

    assert_bundle_is_complete(&outcome, &dir);
    assert_eq!(
        *injector.handed.lock().unwrap(),
        [InjectRequest {
            node: "i1".into(),
            action: "node-kill".into(),
            target: Some("node-b".into())
        }]
    );
    assert!(ack_to_bye_us(&outcome) >= 1_000_000, "{} µs", ack_to_bye_us(&outcome));
    call_ended(scene).await;
}

/// **A refused inject fails the run in the lane's words** (§11.2): the script
/// ends at `i1`, and the close ends the call the script had established.
#[tokio::test(start_paused = true)]
async fn a_refused_inject_fails_the_run_by_name_and_the_close_ends_the_call() {
    let scene = api_scene("pivot-inject-refused").await;
    let refusal = "the operation exited 1".to_string();
    let injector = Arc::new(Scripted::new(500, Some(Err(refusal.clone())), 10_000));
    let case = injected_case("inject-refused", None, None);
    let (outcome, _dir) = replay_injected(&scene, case, injector).await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert_eq!(
        outcome.verdict.failures,
        [Failure::InjectRefused { node: "i1".into(), action: "node-kill".into(), reason: refusal }]
    );
    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    assert_eq!(abandoned.step.as_deref(), Some("i1"));
    let acts: Vec<(&str, CloseOwed)> =
        abandoned.closed.iter().map(|act| (act.leg.as_str(), act.owed)).collect();
    assert_eq!(acts, [("A", CloseOwed::Bye), ("B", CloseOwed::Answer)], "{:#?}", abandoned.closed);
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    call_ended(scene).await;
}

/// **A silent injector fails the run at its bound**: no answer within
/// `within` is `inject-timed-out`, and the close ends the call.
#[tokio::test(start_paused = true)]
async fn an_unanswered_inject_times_out_by_name_and_the_close_ends_the_call() {
    let scene = api_scene("pivot-inject-silent").await;
    let injector = Arc::new(Scripted::new(0, None, 1_500));
    let case = injected_case("inject-silent", None, None);
    let (outcome, _dir) = replay_injected(&scene, case, injector).await;

    assert_eq!(outcome.verdict.status, VerdictStatus::Failed);
    assert_eq!(
        outcome.verdict.failures,
        [Failure::InjectTimedOut {
            node: "i1".into(),
            action: "node-kill".into(),
            within_ms: 1_500
        }]
    );
    let abandoned = outcome.verdict.abandoned.as_ref().expect("the script was abandoned");
    assert_eq!(abandoned.step.as_deref(), Some("i1"));
    let acts: Vec<(&str, CloseOwed)> =
        abandoned.closed.iter().map(|act| (act.leg.as_str(), act.owed)).collect();
    assert_eq!(acts, [("A", CloseOwed::Bye), ("B", CloseOwed::Answer)], "{:#?}", abandoned.closed);
    assert!(outcome.timing.settled_at_ms.is_some(), "the run settled");
    call_ended(scene).await;
}
