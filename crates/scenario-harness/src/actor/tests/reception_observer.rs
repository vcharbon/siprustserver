//! The reception-observation hook: a plan-riding observer sees the typed
//! message of every satisfied reception goal — matcher or not — with the
//! header list exactly as it arrived, and changes nothing when absent.

use sip_message::{EmitOpts, MessageTemplate, Method, TemplateHeader};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::testkit::*;
use crate::actor::*;
use crate::{Harness, OFFER_SDP};

/// One recorded observation: the leg, the goal index, and the observed
/// message's header list flattened VERBATIM — name casing, repeats and comma
/// folds as the assertions want to read them off the wire.
#[derive(Debug, Clone)]
struct Seen {
    role: &'static str,
    step: usize,
    is_request: bool,
    headers: Vec<(String, String)>,
}

/// The observation log + the observer that fills it — the caller-side policy
/// the crate ships no opinion about.
fn recorder() -> (Arc<Mutex<Vec<Seen>>>, ReceptionObserver) {
    let log: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = log.clone();
    let observer: ReceptionObserver = Arc::new(move |ctx: &ReceptionContext<'_>| {
        sink.lock().unwrap().push(Seen {
            role: ctx.role,
            step: ctx.step,
            is_request: matches!(ctx.received, ReceivedMessage::Request(_)),
            headers: ctx
                .received
                .headers()
                .iter()
                .map(|h| (h.name.to_string(), h.value.to_string()))
                .collect(),
        });
    });
    (log, observer)
}

/// Every value of one header name in wire order.
fn values_of<'a>(seen: &'a Seen, name: &str) -> Vec<&'a str> {
    seen.headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .collect()
}

/// The observation of the given kind on the given leg.
fn one(log: &[Seen], role: &str, is_request: bool) -> Seen {
    let hits: Vec<&Seen> =
        log.iter().filter(|s| s.role == role && s.is_request == is_request).collect();
    assert_eq!(hits.len(), 1, "exactly one {role} observation of this kind: {log:#?}");
    hits[0].clone()
}

/// A call whose INVITE and 180 both carry a REPEATED header and a
/// COMMA-FOLDED one: bob's `ExpectRequest` and alice's `ExpectResponse` carry
/// NO matcher, so only the observation hook can reach the messages.
fn folded_header_plan(
    alice: &crate::Agent,
    bob: &crate::Agent,
    reception_observer: Option<ReceptionObserver>,
) -> CallPlan {
    let repeated_and_folded = || {
        vec![
            TemplateHeader::frozen("X-Trace", "leg-a"),
            TemplateHeader::frozen("X-Trace", "leg-b"),
            TemplateHeader::frozen("X-Caps", "alpha, beta"),
        ]
    };
    let mut invite_headers = vec![TemplateHeader::frozen("Content-Type", "application/sdp")];
    invite_headers.extend(repeated_and_folded());
    let invite =
        MessageTemplate::request(Method::Invite, invite_headers, OFFER_SDP.as_bytes().to_vec());
    let ringing = MessageTemplate::response(180, "Ringing", repeated_and_folded(), Vec::new());
    CallPlan {
        actors: vec![
            caller_spec(
                "alice",
                alice,
                ("bob", bob.clone()),
                vec![
                    Goal::new(
                        Barrier::None,
                        GoalStep::InviteTemplate {
                            callee: "bob",
                            plan: None,
                            template: invite,
                            opts: EmitOpts::default(),
                        },
                    ),
                    Goal::new(
                        Barrier::None,
                        GoalStep::ExpectResponse {
                            status: 180,
                            cseq_method: None,
                            body: BodyExpect::Any,
                            early: None,
                            ack_body: None,
                            matcher: None,
                        },
                    ),
                    Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye),
                ],
            ),
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
                        },
                    ),
                    Goal::new(
                        Barrier::None,
                        GoalStep::RespondTemplate {
                            template: ringing,
                            opts: EmitOpts::default(),
                            early: None,
                        },
                    ),
                    Goal::new(
                        Barrier::None,
                        GoalStep::RespondTemplate {
                            template: response_template(200, "OK", true),
                            opts: EmitOpts::default(),
                            early: None,
                        },
                    ),
                ],
            ),
        ],
        plan: vec![established_phase()],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer,
    }
}

/// The hook fires on a matcher-LESS `ExpectRequest` AND a matcher-less
/// `ExpectResponse`, naming the goal each message satisfied, and hands over the
/// full header list: a repeated header stays two entries in wire order and a
/// comma-folded one stays ONE unsplit value.
#[tokio::test(start_paused = true)]
async fn reception_observer_sees_unmatched_request_and_response_headers() {
    let h = Harness::new("actor-reception-observer-headers").describe(
        "a plan-riding reception observer sees the INVITE its ExpectRequest \
         consumed and the 180 its ExpectResponse consumed — repeats and comma \
         folds intact, neither goal carrying a matcher",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let (log, observer) = recorder();
    let call = folded_header_plan(&alice, &bob, Some(observer));
    let obs = ObservedState::new();
    let ctx = CallCtx::new();
    let verdict = run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
    assert!(verdict.is_ok(), "an observer changes nothing: the call must settle OK, got {verdict:?}");

    let log = log.lock().unwrap().clone();
    let invite = one(&log, "bob", true);
    assert_eq!(invite.step, 0, "the INVITE satisfied bob's first goal");
    assert_eq!(
        values_of(&invite, "X-Trace"),
        vec!["leg-a", "leg-b"],
        "a repeated header reaches the hook un-deduped, in wire order: {invite:#?}",
    );
    assert_eq!(
        values_of(&invite, "X-Caps"),
        vec!["alpha, beta"],
        "a comma-folded header reaches the hook as ONE unsplit value: {invite:#?}",
    );
    assert!(
        invite.headers.iter().any(|(n, _)| n == "X-Trace"),
        "the header name keeps its wire casing: {invite:#?}",
    );
    for tier1 in ["Via", "From", "To", "Call-ID", "CSeq"] {
        assert_eq!(values_of(&invite, tier1).len(), 1, "the observed INVITE carries its {tier1}");
    }

    let ringing = one(&log, "alice", false);
    assert_eq!(ringing.step, 1, "the 180 satisfied alice's second goal");
    assert_eq!(
        values_of(&ringing, "X-Trace"),
        vec!["leg-a", "leg-b"],
        "the response's repeated header is un-deduped too: {ringing:#?}",
    );
    assert_eq!(
        values_of(&ringing, "X-Caps"),
        vec!["alpha, beta"],
        "the response's comma fold is unsplit too: {ringing:#?}",
    );
    for tier1 in ["Via", "From", "To", "Call-ID", "CSeq"] {
        assert_eq!(values_of(&ringing, tier1).len(), 1, "the observed 180 carries its {tier1}");
    }

    // Retention: with the hook installed the leg's response facts keep their
    // typed message — what the matcher-less goal above had to hand over.
    let retained =
        obs.with_snapshot(|s| s.leg("alice").responses().iter().filter(|f| f.typed.is_some()).count());
    assert!(retained > 0, "an installed observer retains the typed responses it observes");
    h.finish().await;
}

/// No hook installed: the identical plan runs identically AND retains nothing
/// extra — no goal carries a matcher, so every response fact drops its typed
/// message exactly as before.
#[tokio::test(start_paused = true)]
async fn no_reception_observer_retains_no_typed_response() {
    let h = Harness::new("actor-reception-observer-absent").describe(
        "the same plan without an observer: same verdict, and no response \
         fact retains a typed message",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let call = folded_header_plan(&alice, &bob, None);
    let obs = ObservedState::new();
    let ctx = CallCtx::new();
    let verdict = run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
    assert!(verdict.is_ok(), "the hook-absent plan settles OK, got {verdict:?}");

    let facts = obs.with_snapshot(|s| s.leg("alice").responses().to_vec());
    assert!(!facts.is_empty(), "alice observed responses at all");
    assert!(
        facts.iter().all(|f| f.typed.is_none()),
        "with neither a matcher nor a hook, no typed response is retained: {facts:#?}",
    );
    h.finish().await;
}

/// The hook is observational: a goal whose matcher FAILS still reaches it, and
/// the observer's own return value cannot rescue the run — the matcher's
/// fail-fast verdict stands.
#[tokio::test(start_paused = true)]
async fn reception_observer_fires_when_the_matcher_fails() {
    let h = Harness::new("actor-reception-observer-matcher-fails").describe(
        "an ExpectRequest whose matcher demands an absent header still hands \
         the message to the observer, and still fails the run",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let (log, observer) = recorder();
    let mut call = folded_header_plan(&alice, &bob, Some(observer));
    // Demand a header the INVITE does not carry — the matcher fails.
    call.actors[1].goals[0].step = GoalStep::ExpectRequest {
        kind: RequestKind::Initial,
        body: BodyExpect::Any,
        matcher: Some(MessageTemplate::request(
            Method::Invite,
            vec![TemplateHeader::frozen("X-Absent", "never-sent")],
            Vec::new(),
        )),
    };
    let obs = ObservedState::new();
    let ctx = CallCtx::new();
    let verdict = run_call_with(call, obs, &ctx, Duration::from_secs(5), None).await;
    match &verdict {
        CallVerdict::Failed(StepError::UnexpectedKind { who, detail }) => {
            assert_eq!(who, "bob");
            assert!(detail.contains("did not match its template"), "{detail}");
        }
        other => panic!("the failing matcher must still decide the verdict, got {other:?}"),
    }
    let log = log.lock().unwrap().clone();
    let invite = one(&log, "bob", true);
    assert_eq!(
        values_of(&invite, "X-Trace"),
        vec!["leg-a", "leg-b"],
        "the hook saw the message the matcher rejected: {invite:#?}",
    );
    h.finish().await;
}
