//! The reaped check covers the call limiter.
//!
//! A SUT built without a limiter of its own runs a real limiter store, and
//! every route its decision returns carries one extra non-binding entry, so
//! every routed call holds the limiter and must drain it. `assert_fully_reaped`
//! fails while a hold is unreleased or still counted by the store; a scenario
//! that leaves holds behind on purpose declares them with
//! `assert_fully_reaped_leaving`.

use std::sync::Arc;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallLimiterEntry, CallTreatment, NewCallResponse, RouteDecision, ScriptedDecisionEngine,
};
use b2bua::limiter::{AdmitOutcome, CallLimiter, LimiterEntry, RefreshOutcome};
use b2bua_harness::{
    settle_until, B2buaScene, B2buaSut, LimiterLeak, BOB_PORT, DEFAULT_LIMITER_ID,
};
use call_limiter::wire::AdmitEntry;
use call_limiter::{AdmitResult, CallStore, LimiterConfig, RefreshResult};
use scenario_harness::Harness;
use sip_clock::Clock;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// A limiter backend over a real store that applies every admit and drops
/// every release: the SUT releases its calls, the store keeps counting them.
struct DropsReleases {
    store: Arc<CallStore>,
}

#[async_trait]
impl CallLimiter for DropsReleases {
    async fn admit(
        &self,
        key: &str,
        entries: &[LimiterEntry],
        release_on_refusal: bool,
    ) -> AdmitOutcome {
        let wire: Vec<AdmitEntry> =
            entries.iter().map(|e| AdmitEntry { id: e.id.clone(), limit: e.limit }).collect();
        match self.store.admit(key, &wire, release_on_refusal) {
            AdmitResult::Admitted => AdmitOutcome::Admitted,
            AdmitResult::Rejected { limiter_id } => AdmitOutcome::Rejected { limiter_id },
            AdmitResult::Released => AdmitOutcome::Released,
        }
    }
    async fn release(&self, _key: &str) {}
    async fn refresh(&self, key: &str, ids: &[String]) -> RefreshOutcome {
        match self.store.refresh(key, ids) {
            RefreshResult::Extended => RefreshOutcome::Extended,
            RefreshResult::Reregistered => RefreshOutcome::Reregistered,
            RefreshResult::Released => RefreshOutcome::Released,
            RefreshResult::Dropped => RefreshOutcome::Dropped,
        }
    }
}

fn limiters(ids: &[(&str, i64)]) -> Vec<CallLimiterEntry> {
    ids.iter().map(|(id, limit)| CallLimiterEntry { id: (*id).into(), limit: *limit }).collect()
}

/// A route to bob holding `ids`.
fn limited_route(ids: &[(&str, i64)]) -> RouteDecision {
    let mut r = route_to("127.0.0.1", BOB_PORT);
    r.call_limiter = limiters(ids);
    r
}

fn routes_holding(ids: &'static [(&'static str, i64)]) -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| NewCallResponse::Route(limited_route(ids)))
            .build(),
    )
}

/// One call through a SUT whose limiter drops releases: established, hung up
/// by alice, reaped. Returns the scene for the caller's reaped check.
async fn call_through_a_backend_that_drops_releases(name: &str) -> B2buaScene {
    let store = Arc::new(CallStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let limiter = Arc::new(DropsReleases { store: store.clone() });
    let s = B2buaScene::with_b2bua(name, move |_| {
        B2buaSut::builder(routes_holding(&[("x", 10), ("y", 10)]))
            .limiter(limiter)
            .limiter_store(store)
    })
    .await;
    let mut dialog = s.establish().await;
    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    s
}

#[tokio::test]
#[should_panic(expected = "limiter leak: the store still counts 2 hold(s)")]
async fn a_limiter_backend_that_keeps_released_holds_fails_the_reaped_check() {
    let s = call_through_a_backend_that_drops_releases("reaped-limiter-backend-leak").await;
    s.b2bua.assert_fully_reaped();
}

#[tokio::test]
async fn a_declared_limiter_leak_passes_the_reaped_check() {
    let s = call_through_a_backend_that_drops_releases("reaped-limiter-declared-leak").await;
    let count = s.b2bua.limiter_count();
    assert_eq!((count.admitted, count.released, count.stored), (2, 2, Some(2)));
    let _ = s.finish_leaving(LimiterLeak { unreleased: 0, stored: 2 }).await;
}

/// The call-state checks judge the calls alone: a hold the store still
/// counts is left to the full check.
#[tokio::test]
async fn the_call_state_checks_leave_the_limiter_to_the_full_check() {
    let s = call_through_a_backend_that_drops_releases("reaped-calls-only").await;
    s.b2bua.assert_calls_reaped();
    let _ = s.finish_leaving(LimiterLeak { unreleased: 0, stored: 2 }).await;
}

/// A call still established fails the call-state checks; once it ends they
/// pass.
#[tokio::test]
async fn a_call_not_yet_reaped_fails_the_call_state_checks() {
    let s = B2buaScene::new("reaped-calls-live").await;
    let mut dialog = s.establish().await;
    let live = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        s.b2bua.assert_calls_reaped();
    }))
    .expect_err("an established call fails the call-state checks");
    let message = live.downcast_ref::<String>().map(String::as_str).unwrap_or_default();
    assert!(message.contains("call leak"), "{message}");

    s.hangup(&mut dialog).await;
    let _ = s.finish().await;
}

#[tokio::test]
async fn every_routed_call_holds_the_default_limiter_until_it_ends() {
    let s = B2buaScene::new("reaped-limiter-default").await;
    let mut dialog = s.establish().await;
    let store = s.b2bua.limiter_store().expect("the default limiter has a store");
    assert_eq!(store.held(DEFAULT_LIMITER_ID), 1, "the established call holds the default");
    let count = s.b2bua.limiter_count();
    assert_eq!((count.admitted, count.released, count.stored), (1, 0, Some(1)));

    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    assert_eq!(store.held(DEFAULT_LIMITER_ID), 0, "the hangup releases the default");
    s.b2bua.assert_fully_reaped();
    assert_eq!(s.b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = s.finish().await;
}

/// The route's own entries `[x, x, y]` (the same id twice, a distinct id)
/// are admitted with the default in one set and all released.
#[tokio::test]
async fn a_route_s_own_limiters_are_held_with_the_default() {
    let s = B2buaScene::with_b2bua("reaped-limiter-own-and-default", |_| {
        B2buaSut::builder(routes_holding(&[("x", 10), ("x", 10), ("y", 10)]))
    })
    .await;
    let mut dialog = s.establish().await;
    let store = s.b2bua.limiter_store().expect("the default limiter has a store").clone();
    let held = || ["x", "y", DEFAULT_LIMITER_ID].map(|id| store.held(id));
    assert_eq!(held(), [2, 1, 1], "holds on x, y and the default while the call is up");

    s.hangup(&mut dialog).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    assert_eq!(held(), [0, 0, 0], "the hangup releases every hold");
    s.b2bua.assert_fully_reaped();
    assert_eq!(s.b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = s.finish().await;
}

/// `[x, y(0)]` is refused on its second entry: the set, the default
/// included, counts nothing and the caller gets 486.
#[tokio::test]
async fn a_route_refused_on_its_second_limiter_holds_nothing() {
    let s = B2buaScene::with_b2bua("reaped-limiter-refused", |_| {
        B2buaSut::builder(routes_holding(&[("x", 10), ("y", 0)]))
    })
    .await;
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    call.expect(486).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    let store = s.b2bua.limiter_store().expect("the default limiter has a store");
    assert_eq!(["x", "y", DEFAULT_LIMITER_ID].map(|id| store.held(id)), [0, 0, 0]);
    assert_eq!(s.b2bua.limiter_count().admitted, 0, "nothing granted");
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}

/// Initial `[x, y]`, failover `[y, z]`: the failover route carries the default
/// too, and replaces the initial route's holds, default included.
#[tokio::test]
async fn a_failover_route_holds_its_own_limiters_with_the_default() {
    let h = Harness::new("reaped-limiter-failover");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = limited_route(&[("x", 10), ("y", 10)]);
                r.callback_context = Some("failover-ctx".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_| {
                let mut r = route_to("127.0.0.1", 5071);
                r.call_limiter = limiters(&[("y", 10), ("z", 10)]);
                CallTreatment::Route(r)
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision).start(&h, "b2bua", "127.0.0.1:5080").await;
    let store = b2bua.limiter_store().expect("the default limiter has a store").clone();
    let held = || ["x", "y", "z", DEFAULT_LIMITER_ID].map(|id| store.held(id));

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    assert_eq!(held(), [1, 1, 0, 1], "the initial route's holds while bob is dialed");
    bob_uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;

    let mut carol_uas = carol.receive("INVITE").await;
    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;
    settle_until(|| held() == [0, 1, 1, 1]).await;
    assert_eq!(held(), [0, 1, 1, 1], "the failover route's holds replace the initial ones");

    let mut bye = dialog.bye().await;
    carol.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    assert_eq!(held(), [0, 0, 0, 0], "the hangup releases every hold");
    let count = b2bua.limiter_count();
    assert_eq!(
        (count.admitted, count.released),
        (6, 6),
        "three holds per route: the failover replaced the initial set, the hangup released it"
    );
    b2bua.assert_fully_reaped();
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}
