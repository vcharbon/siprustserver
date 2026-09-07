//! WHICH parked request a reception goal consumes when two of one kind are
//! parked at once: the step's CSeq rank on the leg first, its [`BodyExpect`]
//! next, arrival order last. Arrival-order-only pairing is what turns one extra
//! message of a kind into a mis-pairing of every later one.

use std::time::Duration;

use sip_message::generators::InDialogMethod;
use sip_message::{MessageTemplate, Method, TemplateHeader};

use super::testkit::*;
use crate::actor::*;
use crate::Harness;

/// One INFO carrying `marker`, with `body` when it has one.
fn info(marker: &str, body: Option<&[u8]>) -> GoalStep {
    GoalStep::InDialog {
        method: InDialogMethod::Info,
        content_type: body.map(|_| "application/x-cap-test".to_string()),
        body: body.map(|b| b.to_vec()),
        headers: vec![("X-Marker".to_string(), marker.to_string())],
    }
}

/// A matcher demanding the INFO that carries `marker` and `body` — the pin that
/// says WHICH of the two parked requests the goal consumed.
fn marked(marker: &'static str, body: &[u8]) -> MessageTemplate {
    MessageTemplate::request(
        Method::Info,
        vec![TemplateHeader::frozen("X-Marker", marker)],
        body.to_vec(),
    )
}

/// The callee's script: claim the INVITE, answer it, dwell long enough for BOTH
/// INFOs to park, consume them under `first` then `second`, and answer each.
fn callee(bob: &crate::Agent, first: GoalStep, second: GoalStep) -> ActorSpec {
    scripted_spec(
        "bob",
        bob,
        vec![
            Goal::new(
                Barrier::None,
                GoalStep::ExpectRequest {
                    kind: RequestKind::Initial,
                    body: BodyExpect::SdpPresent,
                    matcher: None,
                    rank: None,
                },
            ),
            Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
            Goal::new(Barrier::None, first).after(Duration::from_millis(100)),
            Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
            Goal::new(Barrier::None, second),
            Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
        ],
    )
}

/// The caller's script: establish, send both INFOs back to back, hang up once
/// both are answered.
fn caller(alice: &crate::Agent, bob: &crate::Agent, one: GoalStep, two: GoalStep) -> ActorSpec {
    caller_spec(
        "alice",
        alice,
        ("bob", bob.clone()),
        vec![
            Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
            Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), one),
            Goal::new(Barrier::None, two),
            Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye)
                .after(Duration::from_millis(400)),
        ],
    )
}

fn plan(caller: ActorSpec, callee: ActorSpec) -> CallPlan {
    CallPlan {
        actors: vec![caller, callee],
        plan: vec![established_phase()],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    }
}

/// The rank steers the pick: with two INFOs parked, the goal ranked `1` takes
/// the SECOND of the leg's INFO transactions (CSeq order) — arrival order alone
/// would hand it the first. The following goal takes what is left.
#[tokio::test(start_paused = true)]
async fn the_cseq_rank_picks_among_two_parked_requests_of_one_kind() {
    let h = Harness::new("actor-request-pick-rank").describe(
        "two INFOs parked: ExpectRequest{Info, rank 1} consumes the leg's \
         SECOND INFO, the next goal the first — both answered, clean teardown",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let call = plan(
        caller(&alice, &bob, info("one", Some(b"one")), info("two", Some(b"two"))),
        callee(
            &bob,
            GoalStep::ExpectRequest {
                kind: RequestKind::InDialog(InDialogMethod::Info),
                body: BodyExpect::Present,
                matcher: Some(marked("two", b"two")),
                rank: Some(1),
            },
            GoalStep::ExpectRequest {
                kind: RequestKind::InDialog(InDialogMethod::Info),
                body: BodyExpect::Present,
                matcher: Some(marked("one", b"one")),
                rank: Some(0),
            },
        ),
    );

    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let verdict = run_call_with(call, obs, &ctx, Duration::from_secs(5), None).await;
    assert!(verdict.is_ok(), "the ranked goals must consume their own INFO, got {verdict:?}");
    h.finish().await;
}

/// With no rank to go on, the body discriminates: `Absent` consumes the
/// BODYLESS parked INFO even though the body-bearing one arrived first, and the
/// `Present` goal that follows takes the other.
#[tokio::test(start_paused = true)]
async fn a_bodyless_expectation_does_not_consume_a_body_bearing_parked_request() {
    let h = Harness::new("actor-request-pick-body").describe(
        "two INFOs parked, one bodyless: ExpectRequest{Info, Absent} consumes \
         the bodyless one out of arrival order, the Present goal the other",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let call = plan(
        caller(&alice, &bob, info("with-body", Some(b"one")), info("bodyless", None)),
        callee(
            &bob,
            GoalStep::ExpectRequest {
                kind: RequestKind::InDialog(InDialogMethod::Info),
                body: BodyExpect::Absent,
                matcher: Some(marked("bodyless", b"")),
                rank: None,
            },
            GoalStep::ExpectRequest {
                kind: RequestKind::InDialog(InDialogMethod::Info),
                body: BodyExpect::Present,
                matcher: Some(marked("with-body", b"one")),
                rank: None,
            },
        ),
    );

    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let verdict = run_call_with(call, obs, &ctx, Duration::from_secs(5), None).await;
    assert!(verdict.is_ok(), "the body must decide the pick, got {verdict:?}");
    h.finish().await;
}
