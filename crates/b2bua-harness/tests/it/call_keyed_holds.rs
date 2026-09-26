//! A call's limiter holds are one set keyed by the call.
//!
//! A route fold (failover, release reroute) replaces the call's set in one
//! admit checked net of the set the call already holds: an id the call keeps
//! or reduces never refuses, only an id it adds is checked against its cap.
//! A refused replacement releases the call's old set (the ended leg is freed
//! before the failure is consulted). A call that ended before a fold's admit
//! lands holds nothing from it; an admit that failed open is never released.
//!
//! Every scenario carries several limiters with one **witness** hold per id,
//! admitted outside the call, so a surplus release reads below the witness
//! instead of vanishing under the store's floor at 0. The store is probed per
//! id while the call is up and drained to the witnesses after it ends.
//!
//! The first tests run on today's `(id, window)` store and state its defects;
//! `contract` states the keyed store and is compiled out (`cfg(any())`) until
//! that store exists.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallLimiterEntry, CallReferRequest, CallReferResponse, CallReleaseRequest, CallReleaseResponse,
    CallTreatment, NewCallRequest, NewCallResponse, ReleaseOutcome, ScriptedDecisionEngine,
};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use b2bua_harness::{invite_final_statuses, settle_until, B2buaSut};
use call::ReleaseEventKind;
use call_limiter::wire::{AdmitEntry, Hold};
use call_limiter::{AdmitResult, LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use scenario_harness::Harness;
use sip_clock::Clock;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const MEDIA_ANSWER: &str = "v=0\r\no=media 7 7 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";
const ALICE_REALIGN: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// Every id a scenario may hold; each carries one witness hold.
const IDS: [&str; 3] = ["x", "y", "z"];

/// A real `LimiterServer` on the simulated HTTP fabric, so every admit is a
/// genuine hold the store counts, with one witness hold per id in [`IDS`].
struct LimiterRig {
    store: Arc<WindowStore>,
    client: Arc<dyn CallLimiter>,
    witness_window: i64,
    _server: Box<dyn HttpServerHandle>,
}

impl LimiterRig {
    /// The holds the call owns on `id`: the store's count less the witness.
    /// Negative = a release matched no hold of the call.
    fn holds(&self, id: &str) -> i64 {
        self.store.held(id) - 1
    }

    fn all_holds(&self) -> [i64; 3] {
        IDS.map(|id| self.holds(id))
    }

    async fn expect_holds(&self, expected: [i64; 3], why: &str) {
        settle_until(|| self.all_holds() == expected).await;
        assert_eq!(self.all_holds(), expected, "holds on {IDS:?}: {why}");
    }

    /// Settle until the call holds nothing, then release the witnesses so the
    /// store reads empty for the reaped check.
    async fn expect_drained(&self, why: &str) {
        self.expect_holds([0, 0, 0], why).await;
        let witnesses: Vec<Hold> =
            IDS.iter().map(|id| Hold { id: (*id).into(), window: self.witness_window }).collect();
        self.store.release(&witnesses);
    }
}

async fn limiter_rig() -> LimiterRig {
    let laddr: SocketAddr = "10.0.0.1:8080".parse().unwrap();
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(WindowStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let witness_window = store.current_window();
    for id in IDS {
        let witness = store.admit(&[AdmitEntry { id: id.into(), limit: 100 }]);
        assert_eq!(witness, AdmitResult::Admitted { window: witness_window }, "witness on {id}");
    }
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let handle = http.serve(laddr, server).await.unwrap();
    // A fail-open budget above the paused-clock HTTP round trip: the detached
    // admit is woken inside a coarse `h.advance`, whose 100 ms chunks a
    // production-sized budget could expire between.
    let client: Arc<dyn CallLimiter> =
        Arc::new(HttpCallLimiter::new(Arc::new(http.clone()), laddr, Duration::from_secs(2)));
    LimiterRig { store, client, witness_window, _server: handle }
}

/// Limiter entries `(id, cap)`. A cap of 2 on an id the call already holds
/// once is exactly the witness plus the call: room for nothing new.
fn limiters(entries: &[(&str, i64)]) -> Vec<CallLimiterEntry> {
    entries.iter().map(|(id, limit)| CallLimiterEntry { id: (*id).into(), limit: *limit }).collect()
}

/// A route toward `host:port` with `entries` as its call limiters and a
/// callback context, so a failure of the leg it dials consults the decision.
fn limited_route(host: &str, port: u16, entries: &[(&str, i64)]) -> b2bua::decision::RouteDecision {
    let mut r = route_to(host, port);
    r.callback_context = Some("failover-ctx".into());
    r.call_limiter = limiters(entries);
    r
}

/// The initial route toward bob (5070) holding `initial`, whose failure
/// fails over to carol (5071) holding `failover`, whatever the failure's
/// origin: a limiter refusal of the failover route re-consults and is
/// refused again, which ends the chain in the stack's 486.
fn one_failover(
    initial: &'static [(&'static str, i64)],
    failover: &'static [(&'static str, i64)],
) -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| NewCallResponse::Route(limited_route("127.0.0.1", 5070, initial)))
            .on_failure(move |_| CallTreatment::Route(limited_route("127.0.0.1", 5071, failover)))
            .build(),
    )
}

/// Bob busies out, the failover route toward carol is admitted, carol
/// answers, alice hangs up. `before` and `after` are the call's holds on
/// [`IDS`] while bob is dialed and once the failover route is applied.
async fn busy_then_failover_answered(
    initial: &'static [(&'static str, i64)],
    failover: &'static [(&'static str, i64)],
    name: &str,
    before: [i64; 3],
    after: [i64; 3],
) {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let rig = limiter_rig().await;
    let b2bua = B2buaSut::builder(one_failover(initial, failover))
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    rig.expect_holds(before, "the initial route's holds while bob is dialed").await;
    bob_uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;

    // ── the failover route replaces the call's set: carol is dialed ────────
    let Some(mut carol_uas) = carol.try_receive_tolerating("INVITE", &[]).await else {
        panic!(
            "the failover route {failover:?} is admitted: the call already holds {initial:?}, \
             so the ids it keeps need no new slot; holds now {:?}",
            rig.all_holds()
        );
    };
    carol_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    rig.expect_holds(after, "the failover route's holds alone while carol rings").await;

    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;
    rig.expect_holds(after, "the failover route's holds alone while the call is up").await;

    let mut bye = dialog.bye().await;
    carol.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    rig.expect_drained("the hangup releases the failover route's holds").await;
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![200],
        "the a-leg INVITE transaction carries exactly one final: the 200",
    );
}

/// Initial `[x, y]`, failover `[y, z]` with `y` at its cap (the witness and
/// the call's own hold fill it): the call keeps `y`, so the replacement is
/// admitted and the call ends up holding `y` and `z` once each.
#[tokio::test(start_paused = true)]
async fn failover_route_keeping_an_id_at_its_cap_is_admitted() {
    busy_then_failover_answered(
        &[("x", 10), ("y", 10)],
        &[("y", 2), ("z", 10)],
        "keyed-holds-failover-kept-id-at-cap",
        [1, 1, 0],
        [0, 1, 1],
    )
    .await;
}

/// Initial `[x, x]`, failover `[x]` with `x` at its cap: the call reduces
/// `x`, which never refuses; it ends up holding `x` once.
#[tokio::test(start_paused = true)]
async fn failover_route_reducing_an_id_at_its_cap_is_admitted() {
    busy_then_failover_answered(
        &[("x", 10), ("x", 10)],
        &[("x", 2)],
        "keyed-holds-failover-reduced-id-at-cap",
        [2, 0, 0],
        [1, 0, 0],
    )
    .await;
}

/// Release reroute of an established call holding `[x, y]` toward a media
/// server with `[y, z]`, `y` at its cap: the call keeps `y`, so the reroute
/// is admitted and applied; the rerouted call holds `y` and `z`.
#[tokio::test(start_paused = true)]
async fn release_reroute_keeping_an_id_at_its_cap_is_admitted() {
    let h = Harness::new("keyed-holds-release-reroute-kept-id-at-cap");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let media = h.agent("media", "127.0.0.1:5090").await;
    let rig = limiter_rig().await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.features.platform.max_duration_sec = 60;
                r.callback_context = Some("release-ctx".into());
                r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
                r.call_limiter = limiters(&[("x", 10), ("y", 10)]);
                NewCallResponse::Route(r)
            })
            .on_release(|_| {
                let mut r = route_to("127.0.0.1", 5090);
                r.call_limiter = limiters(&[("y", 2), ("z", 10)]);
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

    // ── the cap raises the release consult; the reroute is admitted ────────
    h.advance(Duration::from_secs(61)).await;
    let Some(mut media_uas) = media.try_receive_tolerating("INVITE", &[]).await else {
        panic!(
            "the reroute [y, z] is admitted: the call already holds y, so y needs no new \
             slot; holds now {:?}",
            rig.all_holds()
        );
    };
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
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    rig.expect_drained("the hangup releases the reroute's holds").await;
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let _ = h.finish().await;
}

/// Delays the `n`-th `call_failure` (1-based) by `delay` before delegating.
struct DelayNthFailure {
    n: usize,
    delay: Duration,
    failures: AtomicUsize,
    inner: Arc<dyn CallDecisionEngine>,
}

#[async_trait]
impl CallDecisionEngine for DelayNthFailure {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        self.inner.new_call(req).await
    }
    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        if self.failures.fetch_add(1, Ordering::SeqCst) + 1 == self.n {
            tokio::time::sleep(self.delay).await;
        }
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
        self.inner.call_release(req).await
    }
}

/// A refused replacement releases the call's old set. The initial route
/// holds `x`; bob busies out and the failover route `[y, z]` is refused on
/// its second entry (`z` at its cap): nothing is counted, not even `y`, and
/// the call's `x` is released in the same step, before the limiter-reject
/// re-consult answers. The re-consult's route `[z]` (room this time) is
/// applied; the call holds `z` alone and drains on hangup. The witnesses are
/// intact throughout.
#[tokio::test(start_paused = true)]
async fn refused_replacement_releases_the_call_holds() {
    let h = Harness::new("keyed-holds-refused-replacement");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let rig = limiter_rig().await;
    let origins = Arc::new(Mutex::new(Vec::new()));
    let seen = origins.clone();
    let decision = Arc::new(DelayNthFailure {
        n: 2,
        delay: Duration::from_secs(1),
        failures: AtomicUsize::new(0),
        inner: Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_| {
                    NewCallResponse::Route(limited_route("127.0.0.1", 5070, &[("x", 10)]))
                })
                .on_failure(move |req| {
                    seen.lock().unwrap().push(req.failure.origin.clone());
                    if req.failure.origin == "call_limiter" {
                        return CallTreatment::Route(limited_route(
                            "127.0.0.1",
                            5071,
                            &[("z", 10)],
                        ));
                    }
                    // `z` at cap 1 is refused: its witness already holds one.
                    CallTreatment::Route(limited_route("127.0.0.1", 5099, &[("y", 10), ("z", 1)]))
                })
                .build(),
        ),
    });
    let b2bua = B2buaSut::builder(decision)
        .limiter(rig.client.clone())
        .limiter_store(rig.store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    rig.expect_holds([1, 0, 0], "the initial route holds x").await;
    bob_uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;

    // ── the failover route is refused; the re-consult is in flight ─────────
    h.advance(Duration::from_millis(500)).await;
    assert_eq!(
        *origins.lock().unwrap(),
        vec!["external".to_string()],
        "the failover route was answered; the re-consult is still delayed",
    );
    // Read at once: a settle would wait through the delayed re-consult.
    assert_eq!(
        rig.all_holds(),
        [0, 0, 0],
        "holds on {IDS:?}: the refused replacement released x; y and z were never counted",
    );

    // ── the re-consult's route is applied ───────────────────────────────────
    let mut carol_uas = carol.receive("INVITE").await;
    assert_eq!(
        *origins.lock().unwrap(),
        vec!["external".to_string(), "call_limiter".to_string()],
        "the refused route re-consulted with the limiter origin",
    );
    rig.expect_holds([0, 0, 1], "the applied route's z is the call's set").await;
    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;

    let mut bye = dialog.bye().await;
    carol.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    rig.expect_drained("the hangup releases the applied route's hold").await;
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![200],
        "the a-leg INVITE transaction carries exactly one final: the 200",
    );
}

/// The keyed store's contract at the SIP level. Compiled out until the store
/// exists: the fix phase removes the gate and settles the names.
#[cfg(any())]
mod contract {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use b2bua::decision::test_adapter::route_to;
    use b2bua::decision::{
        CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
        CallLimiterEntry, CallReferRequest, CallReferResponse, CallReleaseRequest,
        CallReleaseResponse, CallTreatment, NewCallRequest, NewCallResponse,
        ScriptedDecisionEngine,
    };
    use b2bua::limiter::CallLimiter;
    use b2bua::limiter_http::HttpCallLimiter;
    use b2bua_harness::{invite_final_statuses, settle_until, B2buaSut};
    use call_limiter::wire::AdmitEntry;
    use call_limiter::{AdmitResult, CallStore, LimiterConfig, LimiterMetrics, LimiterServer};
    use http_net::{Fault, HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
    use scenario_harness::Harness;
    use sip_clock::Clock;

    use super::{limiters, ANSWER, IDS, OFFER};

    const LADDR: &str = "10.0.0.1:8080";
    /// The production admit budget.
    const ADMIT_BUDGET: Duration = Duration::from_millis(150);
    /// A short lease, so the paused clock crosses it cheaply.
    const LEASE_SEC: i64 = 20;

    /// A real keyed `LimiterServer` on the simulated HTTP fabric with one
    /// witness hold per id, each under its own call.
    struct KeyedRig {
        http: SimulatedHttpNetwork,
        store: Arc<CallStore>,
        client: Arc<dyn CallLimiter>,
        _server: Box<dyn HttpServerHandle>,
    }

    impl KeyedRig {
        fn holds(&self, id: &str) -> i64 {
            self.store.held(id) - 1
        }

        fn all_holds(&self) -> [i64; 3] {
            IDS.map(|id| self.holds(id))
        }

        async fn expect_holds(&self, expected: [i64; 3], why: &str) {
            settle_until(|| self.all_holds() == expected).await;
            assert_eq!(self.all_holds(), expected, "holds on {IDS:?}: {why}");
        }

        async fn expect_drained(&self, why: &str) {
            self.expect_holds([0, 0, 0], why).await;
            for id in IDS {
                self.store.release(&format!("witness-{id}"));
            }
        }
    }

    async fn keyed_rig(budget: Duration) -> KeyedRig {
        let laddr: SocketAddr = LADDR.parse().unwrap();
        let http = SimulatedHttpNetwork::new();
        let store =
            Arc::new(CallStore::new(LimiterConfig { lease_sec: LEASE_SEC }, Clock::test_at(0)));
        for id in IDS {
            let witness = store.admit(
                &format!("witness-{id}"),
                &[AdmitEntry { id: id.into(), limit: 100 }],
                false,
            );
            assert_eq!(witness, AdmitResult::Admitted, "witness on {id}");
        }
        let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
        let handle = http.serve(laddr, server).await.unwrap();
        let client: Arc<dyn CallLimiter> =
            Arc::new(HttpCallLimiter::new(Arc::new(http.clone()), laddr, budget));
        KeyedRig { http, store, client, _server: handle }
    }

    /// Delays every `call_failure` by `delay` before delegating.
    struct DelayedFailure {
        delay: Duration,
        inner: Arc<dyn CallDecisionEngine>,
    }

    #[async_trait]
    impl CallDecisionEngine for DelayedFailure {
        async fn new_call(
            &self,
            req: NewCallRequest,
        ) -> Result<NewCallResponse, CallDecisionError> {
            self.inner.new_call(req).await
        }
        async fn call_failure(
            &self,
            req: CallFailureRequest,
        ) -> Result<CallFailureResponse, CallDecisionError> {
            tokio::time::sleep(self.delay).await;
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
            self.inner.call_release(req).await
        }
    }

    /// The initial admit times out on the client but lands on the server: the
    /// call runs uncounted, sends no refresh and no release, and the server's
    /// count for it lapses with its lease. The witnesses' leases are kept
    /// alive across it.
    #[tokio::test(start_paused = true)]
    async fn an_admit_that_times_out_and_lands_late_is_never_released() {
        let h = Harness::new("keyed-holds-late-admit-never-released");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        let rig = keyed_rig(ADMIT_BUDGET).await;
        // The limiter answers past the admit budget: the request lands, the
        // client gives up.
        rig.http.apply_fault(Fault::Delay { dst: LADDR.parse().unwrap(), ms: 400 });
        let decision = Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_| {
                    let mut r = route_to("127.0.0.1", 5070);
                    r.call_limiter = limiters(&[("x", 10), ("y", 10)]);
                    NewCallResponse::Route(r)
                })
                .build(),
        );
        let b2bua = B2buaSut::builder(decision)
            .limiter(rig.client.clone())
            .limiter_store(rig.store.clone())
            .tune(|c| c.limiter_refresh_sec = 5)
            .start(&h, "b2bua", "127.0.0.1:5080")
            .await;

        let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
        let mut uas = bob.receive("INVITE").await;
        uas.respond(200, "OK").with_sdp(ANSWER).await;
        call.expect(200).await;
        let mut dialog = call.ack().await;
        bob.receive("ACK").await;
        rig.expect_holds([1, 1, 0], "the late admit counted the call on the server").await;
        rig.http.apply_fault(Fault::Resume { dst: LADDR.parse().unwrap() });

        // Past several refresh periods the uncounted call has refreshed
        // nothing: its server-side set lapses with its lease.
        for _ in 0..LEASE_SEC {
            h.advance(Duration::from_secs(1)).await;
            for id in IDS {
                assert!(rig.store.refresh(&format!("witness-{id}")));
            }
        }
        h.advance(Duration::from_secs(2)).await;
        rig.store.sweep_now();
        rig.expect_holds([0, 0, 0], "the uncounted call's set lapsed with its lease").await;
        assert_eq!(rig.store.stats().lease_expired_calls, 1);

        let mut bye = dialog.bye().await;
        bob.receive("BYE").await.respond(200, "OK").await;
        bye.expect(200).await;
        settle_until(|| b2bua.is_reaped()).await;
        rig.expect_drained("the uncounted call released nothing").await;
        assert_eq!(rig.store.stats().releases_total, 3, "only the witnesses' releases");
        let count = b2bua.limiter_count();
        assert_eq!((count.failed_open, count.released), (1, 0));
        b2bua.assert_fully_reaped();
        let _ = h.finish().await;
    }

    /// A failover fold whose admit reaches the server after the call ended:
    /// the caller CANCELs while the consult is in flight, the call terminates
    /// and releases its set; the fold's admit then lands on the tombstone and
    /// holds nothing; the gone-call path's second release is a no-op. The
    /// witnesses are intact.
    #[tokio::test(start_paused = true)]
    async fn a_fold_admitted_after_the_call_ended_holds_nothing() {
        let h = Harness::new("keyed-holds-fold-after-the-call-ended");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        let carol = h.agent("carol", "127.0.0.1:5071").await;
        let rig = keyed_rig(Duration::from_secs(2)).await;
        let decision = Arc::new(DelayedFailure {
            delay: Duration::from_millis(900),
            inner: Arc::new(
                ScriptedDecisionEngine::builder()
                    .fallback(|_| {
                        let mut r = route_to("127.0.0.1", 5070);
                        r.callback_context = Some("failover-ctx".into());
                        r.call_limiter = limiters(&[("x", 10), ("y", 10)]);
                        NewCallResponse::Route(r)
                    })
                    .on_failure(|_| {
                        let mut r = route_to("127.0.0.1", 5071);
                        r.call_limiter = limiters(&[("y", 10), ("z", 10)]);
                        CallTreatment::Route(r)
                    })
                    .build(),
            ),
        });
        let b2bua = B2buaSut::builder(decision)
            .limiter(rig.client.clone())
            .limiter_store(rig.store.clone())
            .start(&h, "b2bua", "127.0.0.1:5080")
            .await;

        let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
        bob.receive("INVITE").await.respond(486, "Busy Here").await;
        bob.receive("ACK").await;
        rig.expect_holds([1, 1, 0], "the initial route's set").await;

        // ── the caller gives up while the consult is in flight ─────────────
        let mut cxl = call.cancel().await;
        cxl.expect(200).await;
        call.expect(487).await;
        settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total())
            .await;
        rig.expect_holds([0, 0, 0], "the terminated call released its set").await;
        let refused_before = rig.store.stats().admits_refused_released;

        // ── the fold lands on the gone call: its admit hit the tombstone ───
        h.advance(Duration::from_secs(2)).await;
        assert!(
            carol.try_receive_tolerating("INVITE", &[]).await.is_none(),
            "the fold dials no leg for a call that is gone",
        );
        assert_eq!(
            rig.store.stats().admits_refused_released,
            refused_before + 1,
            "the fold's admit was refused by the tombstone",
        );
        rig.expect_drained("the fold's set was never counted; the witnesses are intact").await;
        b2bua.assert_fully_reaped();

        settle_until(|| !b2bua.cdr_records().is_empty()).await;
        assert_eq!(b2bua.cdr_records().len(), 1, "exactly one CDR");
        let report = h.finish().await;
        assert_eq!(invite_final_statuses(&report, alice.addr()), vec![487]);
    }
}
