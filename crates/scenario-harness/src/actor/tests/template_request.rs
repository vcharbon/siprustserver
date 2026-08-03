use sip_message::{EmitOpts, MessageTemplate, Method, TemplateHeader};
use std::time::Duration;

use crate::actor::*;
use super::testkit::*;
use crate::{Harness, ANSWER_SDP, OFFER_SDP};

/// A templated in-dialog re-INVITE (`RequestTemplate`, delayed offer):
/// frozen header rides, the ReInvite obligation opens, the peer's 2xx is
/// ACKed with the answer SDP, and the renegotiation completes — settle
/// clean.
#[tokio::test(start_paused = true)]
async fn request_template_reinvite_completes_renegotiation() {
    let h = Harness::new("actor-request-template-reinvite").describe(
        "RequestTemplate re-INVITE (bodyless, frozen X header) on the \
         confirmed dialog: 2xx ACKed with the answer SDP, reneg completes",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let reinvite_tmpl = MessageTemplate::request(
        Method::Invite,
        vec![TemplateHeader::frozen("X-Renegotiate", "cap-1")],
        Vec::new(),
    );
    let call = CallPlan {
        actors: vec![
            ActorSpec {
                role: "alice",
                agent: alice.clone(),
                disposition: Disposition::Caller,
                media: MediaState::full(OFFER_SDP, ANSWER_SDP),
                goals: vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(
                        Barrier::AllConfirmed(&["alice", "bob"]),
                        GoalStep::RequestTemplate {
                            template: reinvite_tmpl,
                            opts: EmitOpts::default(),
                            early: false,
                        },
                    ),
                    Goal::new(
                        Barrier::pred("reneg_done", |s| s.leg("alice").reneg_count() >= 1),
                        GoalStep::Bye,
                    ),
                ],
                invite_targets: vec![("bob", bob.clone())],
                via: None,
                feed: CtxFeed::default(),
            
                cseq: None,
                delayed: None,
                claim: None,
            },
            ActorSpec {
                role: "bob",
                agent: bob.clone(),
                disposition: Disposition::RingThenAnswer { ring: Duration::from_millis(100) },
                media: MediaState::full(ANSWER_SDP, ANSWER_SDP),
                goals: vec![],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed::default(),
            
                cseq: None,
                delayed: None,
                claim: None,
            },
        ],
        plan: vec![established_phase()],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the templated re-INVITE must complete, got {verdict:?}");
    h.finish().await;
}

/// A templated BYE (`RequestTemplate`): the teardown discharge runs, the
/// Bye obligation opens and its 200 terminates the leg — clean teardown.
#[tokio::test(start_paused = true)]
async fn request_template_bye_tears_down() {
    let h = Harness::new("actor-request-template-bye").describe(
        "RequestTemplate BYE (frozen X header) tears the call down with \
         the semantic Bye goal's bookkeeping",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let bye_tmpl = MessageTemplate::request(
        Method::Bye,
        vec![TemplateHeader::frozen("X-Hangup", "cap-1")],
        Vec::new(),
    );
    let call = CallPlan {
        actors: vec![
            caller_spec(
                "alice",
                &alice,
                ("bob", bob.clone()),
                vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(
                        Barrier::AllConfirmed(&["alice", "bob"]),
                        GoalStep::RequestTemplate {
                            template: bye_tmpl,
                            opts: EmitOpts::default(),
                            early: false,
                        },
                    ),
                ],
            ),
            ActorSpec {
                role: "bob",
                agent: bob.clone(),
                disposition: Disposition::RingThenAnswer { ring: Duration::from_millis(100) },
                media: MediaState::answer(ANSWER_SDP),
                goals: vec![],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed::default(),
            
                cseq: None,
                delayed: None,
                claim: None,
            },
        ],
        plan: vec![established_phase()],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the templated BYE must tear down clean, got {verdict:?}");
    h.finish().await;
}

/// A templated EARLY UPDATE (`RequestTemplate{early: true}`, RFC 3311
/// §5.1): rides the still-pending INVITE's early dialog after the PRACK,
/// its 200 releases the callee's held INVITE 200 — the template twin of
/// the `UpdateEarly` goal.
#[tokio::test(start_paused = true)]
async fn request_template_early_update_rides_early_dialog() {
    let h = Harness::new("actor-request-template-early-update").describe(
        "100rel INVITE → reliable 183 → PRACK → templated EARLY UPDATE \
         (200) → final 200 INVITE → ACK → BYE",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let plan = crate::realcall::InvitePlan {
        via: bob.addr(),
        from: None,
        to: None,
        ruri: None,
        headers: vec![("Supported".to_string(), "100rel".to_string())],
        rewrite: Default::default(),
    };
    let update_tmpl = MessageTemplate::request(
        Method::Update,
        vec![TemplateHeader::frozen("Content-Type", "application/sdp")],
        OFFER_SDP.as_bytes().to_vec(),
    );
    let call = CallPlan {
        actors: vec![
            ActorSpec {
                role: "alice",
                agent: alice.clone(),
                media: MediaState::full(OFFER_SDP, ANSWER_SDP),
                disposition: Disposition::Caller,
                goals: vec![
                    Goal::new(
                        Barrier::None,
                        GoalStep::Invite { callee: "bob", plan: Some(plan) },
                    ),
                    Goal::new(
                        Barrier::pred("early", |s| {
                            s.leg("alice").subflow(SUBFLOW_EARLY).is_some()
                        }),
                        GoalStep::RequestTemplate {
                            template: update_tmpl,
                            opts: EmitOpts::default(),
                            early: true,
                        },
                    ),
                    Goal::new(
                        Barrier::pred("confirmed", |s| {
                            s.leg_at_least("alice", LegPhase::Confirmed)
                        }),
                        GoalStep::Bye,
                    ),
                ],
                invite_targets: vec![("bob", bob.clone())],
                via: None,
                feed: CtxFeed::default(),
            
                cseq: None,
                delayed: None,
                claim: None,
            },
            ActorSpec {
                role: "bob",
                agent: bob.clone(),
                disposition: Disposition::ReliableAnswerEarlyUpdate,
                media: MediaState::answer(ANSWER_SDP),
                goals: vec![],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed::default(),
            
                cseq: None,
                delayed: None,
                claim: None,
            },
        ],
        plan: vec![phase("confirmed", |s| s.leg_at_least("alice", LegPhase::Confirmed))],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the templated early UPDATE must settle OK, got {verdict:?}");
    h.finish().await;
}

/// A TEMPLATED re-INVITE drawn into a §14.1 glare: both ends offer at
/// once, both get 491 — the templated transaction's retained handle
/// hop-ACKs the 491 (`sent_reinvite_txns`), the back-off retries resolve,
/// and both rounds complete.
#[tokio::test(start_paused = true)]
async fn request_template_reinvite_glare_491_hop_acks_and_retries() {
    let h = Harness::new("actor-request-template-glare").describe(
        "templated re-INVITE × bob re-INVITE at once → 491 both ways \
         (templated txn hop-ACKs) → back-off retries → both complete → BYE",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let reinvite_tmpl = MessageTemplate::request(
        Method::Invite,
        vec![TemplateHeader::frozen("X-Renegotiate", "cap-1")],
        Vec::new(),
    );
    let both_confirmed = Barrier::AllConfirmed(&["alice", "bob"]);
    let both_reneged = Barrier::pred("glare_resolved", |s| {
        s.leg("alice").reneg_count() >= 1 && s.leg("bob").reneg_count() >= 1
    });
    let call = CallPlan {
        actors: vec![
            ActorSpec {
                role: "alice",
                agent: alice.clone(),
                media: MediaState::full(OFFER_SDP, ANSWER_SDP),
                disposition: Disposition::Caller,
                goals: vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(
                        both_confirmed.clone(),
                        GoalStep::RequestTemplate {
                            template: reinvite_tmpl,
                            opts: EmitOpts::default(),
                            early: false,
                        },
                    ),
                    Goal::new(both_reneged.clone(), GoalStep::Bye),
                ],
                invite_targets: vec![("bob", bob.clone())],
                via: None,
                feed: CtxFeed::default(),
            
                cseq: None,
                delayed: None,
                claim: None,
            },
            ActorSpec {
                role: "bob",
                agent: bob.clone(),
                disposition: Disposition::RingThenAnswer { ring: Duration::from_millis(100) },
                media: MediaState::full(ANSWER_SDP, ANSWER_SDP),
                goals: vec![Goal::new(both_confirmed.clone(), GoalStep::Reinvite)],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed::default(),
            
                cseq: None,
                delayed: None,
                claim: None,
            },
        ],
        plan: vec![
            established_phase(),
            phase("glare_resolved", |s| {
                s.leg("alice").reneg_count() >= 1 && s.leg("bob").reneg_count() >= 1
            }),
        ],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(10)).await;
    assert!(verdict.is_ok(), "the templated glare must resolve via §14.1, got {verdict:?}");
    h.finish().await;
}
