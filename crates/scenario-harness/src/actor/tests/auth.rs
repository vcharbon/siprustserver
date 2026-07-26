use std::time::Duration;

use crate::actor::*;
use crate::{Harness, ANSWER_SDP, OFFER_SDP};

/// A static-credential [`ChallengeResponder`] (the deferred seam's simplest
/// implementation — real digest would hash the challenge). Returns a fixed
/// credential for any challenge; records what it was asked so the auth-path
/// inputs can be asserted.
struct FakeResponder {
    credential: String,
    seen: std::sync::Mutex<Vec<(u16, String, String)>>,
}
impl FakeResponder {
    fn new(credential: &str) -> Self {
        Self { credential: credential.to_string(), seen: std::sync::Mutex::new(Vec::new()) }
    }
}
impl crate::realcall::ChallengeResponder for FakeResponder {
    fn respond(
        &self,
        challenge: &crate::realcall::Challenge,
        method: &str,
        ruri: &str,
    ) -> Option<String> {
        self.seen
            .lock()
            .unwrap()
            .push((challenge.status, method.to_string(), ruri.to_string()));
        Some(self.credential.clone())
    }
}

/// The actor caller wires the deferred-auth adapter onto its establishing
/// INVITE (RFC 3261 §22.2): a challenging UAS `401`s the first INVITE, alice's
/// reactor ACKs it, adds the responder's credential, resends ONCE (bumped
/// CSeq, fresh branch), and the authenticated INVITE is admitted — the retry
/// is invisible above the reactor (she reaches `Confirmed` and BYEs). This
/// recovers the deleted linear `admitted_uas_retries_through_a_challenging_
/// middlebox` intent against the actor runner.
#[tokio::test(start_paused = true)]
async fn actor_caller_retries_through_a_401_challenge() {
    let h = Harness::new("actor-auth-retry").describe(
        "The actor caller honours env.challenge_responder: a UAS 401s alice's \
         INVITE, her reactor ACKs + credentials + resends once (§22.2), the \
         authed INVITE is admitted, and the call establishes and tears down OK",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let responder = Arc::new(FakeResponder::new(
        "Digest username=\"alice\", realm=\"sip\", nonce=\"n1\", response=\"deadbeef\"",
    ));
    let seen = responder.clone();

    fn alice_confirmed(s: &StateInner) -> bool {
        s.leg_at_least("alice", LegPhase::Confirmed)
    }

    // The challenging UAS: 401 the first INVITE, read its ACK, then admit the
    // authenticated resend (180/200), read its ACK, and answer the BYE.
    let bob_srv = bob.clone();
    let server = tokio::spawn(async move {
        use sip_message::header::HeaderName;
        let bob = bob_srv;
        let mut c = bob.try_receive("INVITE").await.unwrap();
        assert_eq!(c.request().cseq.seq, 1, "the first INVITE is CSeq 1");
        assert!(
            c.request().raw(HeaderName::Authorization).next().is_none(),
            "the first INVITE carries no credential",
        );
        c.respond(401, "Unauthorized")
            .with_header("WWW-Authenticate", "Digest realm=\"sip\", nonce=\"n1\"")
            .try_send()
            .await
            .unwrap();
        // The reactor auto-ACKs the 401 (§17.1.1.3) before the resend.
        let ack = bob.try_receive("ACK").await.unwrap();
        assert_eq!(ack.request().cseq.seq, 1, "the non-2xx ACK reuses the INVITE CSeq");
        // The authenticated resend: bumped CSeq + the responder's credential.
        let mut admit = bob.try_receive("INVITE").await.unwrap();
        assert_eq!(admit.request().cseq.seq, 2, "the retried INVITE bumps the CSeq (§22.2)");
        assert!(
            admit
                .request()
                .raw(HeaderName::Authorization)
                .next()
                .is_some_and(|v| v.starts_with("Digest ")),
            "the retried INVITE carries the responder's Authorization",
        );
        admit.respond(180, "Ringing").try_send().await.unwrap();
        admit.respond(200, "OK").with_sdp(ANSWER_SDP).try_send().await.unwrap();
        bob.try_receive("ACK").await.unwrap();
        // Alice hangs up once established.
        bob.try_receive("BYE").await.unwrap().respond(200, "OK").try_send().await.unwrap();
    });

    let call = CallPlan {
        actors: vec![ActorSpec {
            role: "alice",
            agent: alice.clone(),
            disposition: Disposition::Caller,
            media: MediaState::offer(OFFER_SDP),
            goals: vec![
                Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                Goal::new(Barrier::pred("confirmed", alice_confirmed), GoalStep::Bye),
            ],
            invite_targets: vec![("bob", bob.clone())],
            via: None,
            feed: CtxFeed::default(),
        
            cseq: None,
            delayed: None,
        }],
        plan: vec![phase("confirmed", alice_confirmed)],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
    };

    let ctx = CallCtx::new();
    let verdict = run_call_with(
        call,
        ObservedState::new(),
        &ctx,
        Duration::from_secs(5),
        Some(responder),
    )
    .await;
    assert!(
        verdict.is_ok(),
        "the challenged call must retry, admit, and settle OK, got {verdict:?}",
    );

    // The responder saw the challenge status + the request-line inputs.
    {
        let seen = seen.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "exactly one retry (§22.2 caps at one)");
        assert_eq!(seen[0].0, 401);
        assert_eq!(seen[0].1, "INVITE");
        assert!(seen[0].2.starts_with("sip:bob@"), "ruri passed through: {}", seen[0].2);
    }

    server.await.unwrap();
    h.finish().await;
}

/// WITHOUT a responder (the default) a `401`/`407` to the actor caller's
/// establishing INVITE classifies UNCHANGED: the reactor records the shed
/// final and surfaces `WrongStatus { who: "alice", expected: 180, got: 401 }`
/// (→ `status_401`, `who: "alice"` — the bounded-`who` contract), marking the leg/scope
/// terminated with nothing to CANCEL (the challenged INVITE transaction is
/// complete). Recovers the deleted linear `admitted_uas_without_responder_
/// classifies_401_unchanged` intent against the actor runner.
#[tokio::test(start_paused = true)]
async fn actor_caller_without_responder_classifies_401_unchanged() {
    let h = Harness::new("actor-auth-no-responder").describe(
        "No responder (the default): a 401 to alice's INVITE surfaces as \
         WrongStatus{who:alice, expected:180, got:401} — the challenge is a \
         counted deviation, unchanged, with nothing to CANCEL",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    // The challenger 401s and — since no retry follows — only reads the ACK
    // the reactor auto-sends (§17.1.1.3), so the reject transaction completes
    // on the recorded trace (rfc3261.unackedInviteNon2xxFinal gates).
    let bob_srv = bob.clone();
    let server = tokio::spawn(async move {
        let bob = bob_srv;
        let mut c = bob.try_receive("INVITE").await.unwrap();
        c.respond(401, "Unauthorized")
            .with_header("WWW-Authenticate", "Digest realm=\"sip\", nonce=\"n1\"")
            .try_send()
            .await
            .unwrap();
        bob.try_receive("ACK").await.unwrap();
    });

    fn alice_confirmed(s: &StateInner) -> bool {
        s.leg_at_least("alice", LegPhase::Confirmed)
    }
    let call = CallPlan {
        actors: vec![ActorSpec {
            role: "alice",
            agent: alice.clone(),
            disposition: Disposition::Caller,
            media: MediaState::offer(OFFER_SDP),
            // A pending goal (the would-be BYE) makes the shed an INCIDENTAL
            // establishment failure → the linear `establish` WrongStatus, not
            // an intended terminal.
            goals: vec![
                Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                Goal::new(Barrier::pred("confirmed", alice_confirmed), GoalStep::Bye),
            ],
            invite_targets: vec![("bob", bob.clone())],
            via: None,
            feed: CtxFeed::default(),
        
            cseq: None,
            delayed: None,
        }],
        plan: vec![phase("confirmed", alice_confirmed)],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
    };

    let ctx = CallCtx::new();
    let verdict = run_call_with(
        call,
        ObservedState::new(),
        &ctx,
        Duration::from_secs(5),
        None,
    )
    .await;
    match verdict {
        CallVerdict::Failed(StepError::WrongStatus { who, expected, got, .. }) => {
            assert_eq!((who.as_str(), expected, got), ("alice", 180, 401));
        }
        other => panic!("expected WrongStatus{{who:alice, 180/401}}, got {other:?}"),
    }

    server.await.unwrap();
    h.finish().await;
}

// -----------------------------------------------------------------------
// Capture-replay verbs: template emission, Scripted park-or-react,
// reception goals, lane automatics.
// -----------------------------------------------------------------------


