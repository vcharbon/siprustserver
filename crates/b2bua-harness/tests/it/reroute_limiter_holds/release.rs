//! A release reroute's limiter set replaces the established call's (the
//! parent module states the shared rules and the witness rig).

use super::*;

/// Release reroute of an established call: the route `[x, y]` is replaced by
/// the reroute `[y, z]` toward a media server. Once the reroute is applied the
/// call holds `y` and `z` once each; the hangup releases them.
#[tokio::test(start_paused = true)]
async fn release_reroute_replaces_the_route_holds() {
    let h = Harness::new("reroute-holds-release-reroute");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let media = h.agent("media", "127.0.0.1:5090").await;
    let rig = limiter_rig().await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.features.platform.max_duration_ms = 60_000;
                r.callback_context = Some("release-ctx".into());
                r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
                r.call_limiter = limiters(&["x", "y"]);
                NewCallResponse::Route(r)
            })
            .on_release(|_| {
                let mut r = route_to("127.0.0.1", 5090);
                r.call_limiter = limiters(&["y", "z"]);
                ReleaseOutcome::Respond(CallReleaseResponse::Route(r))
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    // ── establish A↔B ───────────────────────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    rig.expect_holds([1, 1, 0], "the established call holds x and y").await;

    // ── the cap raises the release consult; the reroute is applied ─────────
    h.advance(Duration::from_secs(61)).await;
    let mut media_uas = media.receive("INVITE").await;
    rig.expect_holds([0, 1, 1], "the reroute's holds alone once it is applied").await;
    media_uas.respond(200, "OK").with_sdp(MEDIA_ANSWER).await;
    let tag = media_uas.dialog().local_tag().to_string();
    while let Some(mut retrans) = media.try_receive_tolerating("INVITE", &[]).await {
        retrans.respond(200, "OK").with_sdp(MEDIA_ANSWER).with_to_tag(&tag).await;
    }
    media.receive("ACK").await;

    // The a-leg is re-INVITEd onto the media answer; the displaced b-leg is BYEd.
    let mut realign = alice.receive("INVITE").await;
    realign.respond(200, "OK").with_sdp(ALICE_REALIGN).await;
    alice.receive("ACK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    rig.expect_holds([0, 1, 1], "the rerouted call holds y and z").await;

    // ── the rerouted call ends normally ─────────────────────────────────────
    let mut bye = dialog.bye().await;
    media.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    rig.expect_drained("the hangup releases the reroute's holds").await;
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}

/// An established call holding `[x]` whose duration cap raises a release
/// reroute stating `reroute`; the reroute's admit lands on the limiter and its
/// answer is lost. The call stays counted on `[x]` and the reroute is applied;
/// the rerouted call then outlives two leases (refreshing every 5 s) and
/// `held` is what the limiter holds for it throughout. No set lapses and the
/// hangup's one release drains the call. Returns, read before the hangup, the
/// store's re-registrations and the SUT's refreshes answered `Dropped`.
async fn release_reroute_admit_answer_lost(
    name: &str,
    reroute: &'static [&'static str],
    held: [i64; 3],
) -> (u64, u64) {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let media = h.agent("media", "127.0.0.1:5090").await;
    let rig = limiter_rig_with(LimiterConfig { lease_sec: SHORT_LEASE_SEC }).await;
    let limiter = unavailable_on_nth(2, true, rig.client.clone());
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.features.platform.max_duration_ms = 60_000;
                r.callback_context = Some("release-ctx".into());
                r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
                r.call_limiter = limiters(&["x"]);
                NewCallResponse::Route(r)
            })
            .on_release(move |_| {
                let mut r = route_to("127.0.0.1", 5090);
                r.call_limiter = limiters(reroute);
                ReleaseOutcome::Respond(CallReleaseResponse::Route(r))
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter)
        .limiter_store(rig.store.clone())
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
            c.limiter_refresh_sec = 5;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    rig.expect_holds([1, 0, 0], "the established call holds x").await;

    // ── the cap raises the release consult; the reroute is applied ─────────
    for _ in 0..61 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
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
    rig.expect_holds(held, "the set the limiter holds for the rerouted call").await;

    // ── the rerouted call outlives two leases ───────────────────────────────
    for _ in 0..2 * SHORT_LEASE_SEC + 5 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    rig.store.sweep_now();
    assert_eq!(rig.all_holds(), held, "the limiter holds the same set two leases on");
    assert_eq!(rig.store.stats().lease_expired_calls, 0, "no set lapsed");
    let reregistered = rig.store.stats().reregistered_calls;
    let refresh_dropped = b2bua.metrics().limiter().refresh_answers_total(RefreshOutcome::Dropped);

    let mut bye = dialog.bye().await;
    media.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    settle_until(|| rig.store.stats().releases_total == 1).await;
    assert_eq!(rig.store.stats().releases_total, 1, "one release of the call");
    rig.expect_drained("the hangup's release frees what the limiter holds").await;
    assert_eq!(rig.store.stats().lease_expired_calls, 0, "freed by its release, not its lease");
    let count = b2bua.limiter_count();
    assert_eq!((count.admitted, count.released, count.failed_open), (1, 1, 1));
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
    (reregistered, refresh_dropped)
}

/// A release reroute `[y, z]` whose admit lands with its answer lost: the
/// call stays counted, and its refresh (naming `[x]`) extends the `[y, z]` the
/// limiter holds for it until its release.
#[tokio::test(start_paused = true)]
async fn release_reroute_admit_that_lands_but_times_out_keeps_the_call_counted() {
    let (reregistered, refresh_dropped) = release_reroute_admit_answer_lost(
        "reroute-holds-release-reroute-answer-lost",
        &["y", "z"],
        [0, 1, 1],
    )
    .await;
    assert_eq!(reregistered, 0, "the landed set was extended, never re-created");
    assert_eq!(refresh_dropped, 0);
}

/// A release reroute stating no limiter whose admit lands with its answer
/// lost: the limiter dropped the call's `[x]` behind a drop fence. The call's
/// next refresh learns the set was dropped and the call goes uncounted, so `x`
/// is never re-registered, not even once the fence lapses; the hangup still
/// sends the call's one release.
#[tokio::test(start_paused = true)]
async fn release_reroute_admit_that_dropped_the_set_is_never_re_registered() {
    let (reregistered, refresh_dropped) = release_reroute_admit_answer_lost(
        "reroute-holds-release-reroute-dropped-answer-lost",
        &[],
        [0, 0, 0],
    )
    .await;
    assert_eq!(reregistered, 0, "the dropped set was never re-created");
    assert_eq!(refresh_dropped, 1, "one refresh learnt the drop; the call refreshed no more");
}

/// An established call whose initial admit of `[x]` landed with its answer
/// lost: it runs uncounted while the limiter holds `x` for it. Its duration
/// cap raises a release reroute naming no limiter, whose empty replacement
/// frees `x` as soon as the reroute is applied, not at the lease's end.
#[tokio::test(start_paused = true)]
async fn release_reroute_naming_no_limiter_frees_a_set_whose_answer_was_lost() {
    let h = Harness::new("reroute-holds-release-reroute-lost-answer-then-no-limiter");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let media = h.agent("media", "127.0.0.1:5090").await;
    let rig = limiter_rig().await;
    let limiter = spy(unavailable_on_nth(1, true, rig.client.clone()));
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.features.platform.max_duration_ms = 60_000;
                r.callback_context = Some("release-ctx".into());
                r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
                r.call_limiter = limiters(&["x"]);
                NewCallResponse::Route(r)
            })
            .on_release(|_| {
                ReleaseOutcome::Respond(CallReleaseResponse::Route(route_to("127.0.0.1", 5090)))
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter.clone())
        .limiter_store(rig.store.clone())
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    rig.expect_holds([1, 0, 0], "the limiter holds x for the uncounted call").await;

    // ── the cap raises the release consult; the reroute is applied ─────────
    h.advance(Duration::from_secs(61)).await;
    let mut media_uas = media.receive("INVITE").await;
    rig.expect_holds([0, 0, 0], "the reroute's empty replacement freed x").await;
    assert_eq!(replacing(&limiter), 1, "the reroute's empty replacement left");
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
    rig.expect_holds([0, 0, 0], "the rerouted call holds nothing").await;

    // ── the rerouted call ends normally ─────────────────────────────────────
    let mut bye = dialog.bye().await;
    media.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    rig.expect_drained("the hangup leaves nothing behind").await;
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}

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

/// A release consult in flight is the call's only one: the cap that raised it
/// is spent, and nothing the established call does while the consult is
/// pending raises another. The consult of `[x, y]`'s call answers 150 s late
/// with the reroute `[y, z]`; meanwhile the cap's period elapses twice more,
/// the call refreshes its set and the caller re-INVITEs. One consult, one
/// replacing admit: the route the call applies is the set the limiter holds,
/// and the hangup drains it.
#[tokio::test(start_paused = true)]
async fn release_consult_in_flight_is_the_only_one_of_the_call() {
    const CAP_SEC: i64 = 60;
    const CONSULT_SEC: u64 = 150;
    let h = Harness::new("reroute-holds-release-consult-in-flight");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let media = h.agent("media", "127.0.0.1:5090").await;
    let rig = limiter_rig().await;
    let limiter = spy(rig.client.clone());
    let releases = Arc::new(AtomicUsize::new(0));
    let decision = Arc::new(SlowRelease {
        delay: Duration::from_secs(CONSULT_SEC),
        releases: releases.clone(),
        inner: Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_| {
                    let mut r = route_to("127.0.0.1", 5070);
                    r.features.platform.max_duration_ms = CAP_SEC * 1000;
                    r.callback_context = Some("release-ctx".into());
                    r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
                    r.call_limiter = limiters(&["x", "y"]);
                    NewCallResponse::Route(r)
                })
                .on_release(|_| {
                    let mut r = route_to("127.0.0.1", 5090);
                    r.call_limiter = limiters(&["y", "z"]);
                    ReleaseOutcome::Respond(CallReleaseResponse::Route(r))
                })
                .build(),
        ),
    });
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter.clone())
        .limiter_store(rig.store.clone())
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
            c.call_control_timeout_ms = 1_000 * (CONSULT_SEC as i64 + 30);
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    // ── establish A↔B ───────────────────────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    rig.expect_holds([1, 1, 0], "the established call holds x and y").await;

    // ── the cap raises the release consult, which stays pending ─────────────
    for _ in 0..CAP_SEC + 1 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    settle_until(|| releases.load(Ordering::SeqCst) == 1).await;

    // The caller refreshes the session while the consult is pending.
    let mut reinv = dialog.reinvite(Some(ALICE_REALIGN)).await;
    let reinvite_cseq = dialog.local_cseq();
    let mut bob_reinv = bob.receive("INVITE").await;
    bob_reinv.respond(200, "OK").with_sdp(ANSWER).await;
    reinv.expect(200).await;
    dialog.ack_for(reinvite_cseq, None).await;
    bob.receive("ACK").await;

    // Two more cap periods pass and the call refreshes its set.
    for _ in 0..2 * CAP_SEC + 10 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
    assert_eq!(releases.load(Ordering::SeqCst), 1, "one release consult while it is pending");
    assert_eq!(replacing(&limiter), 0, "no replacing admit before the consult answers");
    rig.expect_holds([1, 1, 0], "the call still holds x and y").await;

    // ── the consult answers; the reroute is applied ─────────────────────────
    for _ in 0..30 {
        h.advance(Duration::from_secs(1)).await;
        rig.refresh_witnesses();
    }
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
    rig.expect_holds([0, 1, 1], "the applied reroute is the set the limiter holds").await;
    assert_eq!(releases.load(Ordering::SeqCst), 1, "one release consult for the call");
    assert_eq!(replacing(&limiter), 1, "one replacing admit for the call");

    // ── the rerouted call ends normally ─────────────────────────────────────
    let mut bye = dialog.bye().await;
    media.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    rig.expect_drained("the hangup releases the reroute's holds").await;
    let count = b2bua.limiter_count();
    assert_eq!((count.admitted, count.released, count.failed_open), (4, 4, 0));
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}
