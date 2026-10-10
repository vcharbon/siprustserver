//! A release event's consult is answered by its deadline, and answered once.
//!
//! The duration cap raises the consult; the consult's answer deadline is the
//! decision deadline plus the reroute's admit budget plus the margin, so a
//! reroute answered late in its decision budget whose admit runs long still
//! lands before the cap fires again. A release fold no consult awaits — a
//! duplicate, or one landing on a reroute already in progress — reaches no
//! rule: the call dials one replacement leg.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, CallReleaseRequest, CallReleaseResponse, NewCallRequest,
    NewCallResponse, ReleaseOutcome, ScriptedDecisionEngine,
};
use b2bua::limiter::LimiterEntry;
use b2bua_harness::limiter::doubles::{admit_all, delay_nth_admit};
use b2bua_harness::{settle_until, B2buaSut};
use call::ReleaseEventKind;
use scenario_harness::{Agent, ClientInvite, Harness};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const MEDIA_ANSWER: &str = "v=0\r\no=media 7 7 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const ALICE_REALIGN: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const FROM_TAG: &str = "alice-tag";
const CAP_SEC: i64 = 60;

/// Counts `call_release` consults and answers each one `delay` late.
struct SlowRelease {
    delay: Duration,
    releases: Arc<AtomicUsize>,
    inner: Arc<dyn CallDecisionEngine>,
}

#[async_trait]
impl CallDecisionEngine for SlowRelease {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        self.inner.new_call(req).await
    }
    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        self.inner.call_failure(req).await
    }
    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.inner.call_refer(req).await
    }
    async fn call_release(
        &self,
        req: CallReleaseRequest,
    ) -> Result<CallReleaseResponse, CallDecisionError> {
        self.releases.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.inner.call_release(req).await
    }
}

/// A route toward bob under a `CAP_SEC` cap subscribed to its release, whose
/// release answers a reroute toward the media server; each consult answers
/// `delay` late.
fn rerouting(delay: Duration, releases: Arc<AtomicUsize>) -> Arc<dyn CallDecisionEngine> {
    let limiter = || vec![LimiterEntry { id: "trunk".into(), limit: 10 }];
    Arc::new(SlowRelease {
        delay,
        releases,
        inner: Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(move |_| {
                    let mut r = route_to("127.0.0.1", 5070);
                    r.features.platform.max_duration_sec = CAP_SEC;
                    r.callback_context = Some("release-ctx".into());
                    r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
                    r.call_limiter = limiter();
                    NewCallResponse::Route(r)
                })
                .on_release(move |_| {
                    let mut r = route_to("127.0.0.1", 5090);
                    r.call_limiter = limiter();
                    ReleaseOutcome::Respond(CallReleaseResponse::Route(r))
                })
                .build(),
        ),
    })
}

/// Alice calls bob through `b2bua`, bob answers.
async fn established(
    alice: &Agent,
    bob: &Agent,
    b2bua: &B2buaSut,
    call_id: &str,
) -> scenario_harness::Dialog {
    let mut call: ClientInvite = alice
        .invite(bob)
        .identity(call_id, FROM_TAG)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    dialog
}

/// The media server answers the replacement leg; alice is realigned onto it
/// and bob released.
async fn reroute_completes(alice: &Agent, bob: &Agent, media: &Agent) {
    let mut media_uas = media.receive("INVITE").await;
    media_uas.respond(200, "OK").with_sdp(MEDIA_ANSWER).await;
    let tag = media_uas.dialog().local_tag().to_string();
    while let Some(mut retrans) = media.try_receive_tolerating("INVITE", &[]).await {
        retrans.respond(200, "OK").with_sdp(MEDIA_ANSWER).with_to_tag(&tag).await;
    }
    media.receive("ACK").await;
    let mut realign = alice.receive("INVITE").await;
    realign.respond(200, "OK").with_sdp(ALICE_REALIGN).await;
    alice.receive("ACK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
}

/// The consult answers a reroute 4.9 s into its 5 s decision budget and the
/// reroute's admit takes 2.5 s: the fold lands 7.4 s after the cap fired,
/// inside the consult's answer deadline, so the cap raises no second consult
/// and the call dials one replacement leg.
#[tokio::test(start_paused = true)]
async fn a_reroute_whose_admit_runs_long_is_answered_by_the_consults_deadline() {
    let h = Harness::new("release-consult-admit-runs-long");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let media = h.agent("media", "127.0.0.1:5090").await;
    let releases = Arc::new(AtomicUsize::new(0));
    let decision = rerouting(Duration::from_millis(4_900), releases.clone());
    let limiter = delay_nth_admit(2, Duration::from_millis(2_500), admit_all());
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter)
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
            c.call_control_timeout_ms = 5_000;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut dialog = established(&alice, &bob, &b2bua, "release-consult-long@127.0.0.1").await;
    h.advance(Duration::from_secs(CAP_SEC as u64 + 9)).await;
    reroute_completes(&alice, &bob, &media).await;
    h.advance(Duration::from_secs(20)).await;
    assert_eq!(releases.load(Ordering::SeqCst), 1, "one release consult for the call");
    assert!(
        media.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "one replacement leg is dialled",
    );

    let mut bye = dialog.bye().await;
    media.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}

/// A release fold landing while the reroute it would start is already in
/// progress — a duplicate answer of the call's consult — reaches no rule:
/// the reroute in progress completes and no second replacement leg is
/// dialled.
#[tokio::test(start_paused = true)]
async fn a_release_fold_no_consult_awaits_reaches_no_rule() {
    const CALL_ID: &str = "release-fold-duplicate@127.0.0.1";
    let h = Harness::new("release-fold-duplicate");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let media = h.agent("media", "127.0.0.1:5090").await;
    let releases = Arc::new(AtomicUsize::new(0));
    let decision = rerouting(Duration::ZERO, releases.clone());
    let b2bua = B2buaSut::builder(decision)
        .limiter(admit_all())
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    let call_ref = call::derive_call_ref("w0", CALL_ID, FROM_TAG);

    let mut dialog = established(&alice, &bob, &b2bua, CALL_ID).await;
    h.advance(Duration::from_secs(CAP_SEC as u64 + 1)).await;
    let mut media_uas = media.receive("INVITE").await;

    // The same answer again, while the replacement leg rings.
    b2bua.post_event(b2bua::CallEvent::InternalEvent {
        call_ref: call_ref.clone(),
        topic: "call-release-result".into(),
        outcome: "reroute".into(),
        payload: serde_json::json!({
            "destination": { "host": "127.0.0.1", "port": 5090 },
            "features": b2bua::decision::default_platform_features(),
            "subscriptions": [],
        }),
        body: Vec::new(),
        incarnation: None,
    });
    h.advance(Duration::from_millis(500)).await;
    assert!(
        media.try_receive_tolerating("INVITE", &[]).await.is_none(),
        "no second replacement leg",
    );

    media_uas.respond(200, "OK").with_sdp(MEDIA_ANSWER).await;
    media.receive("ACK").await;
    let mut realign = alice.receive("INVITE").await;
    realign.respond(200, "OK").with_sdp(ALICE_REALIGN).await;
    alice.receive("ACK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    assert_eq!(releases.load(Ordering::SeqCst), 1, "one release consult for the call");

    let mut bye = dialog.bye().await;
    media.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
