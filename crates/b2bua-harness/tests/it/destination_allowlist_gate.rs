//! Wiring/regression test for the b-leg **destination allow-list** gate
//! (`b2bua::destination_allowlist`, port of `TargetAdmission.ts` + the `applyRoute.ts`
//! admission block). The pure classifier is unit-tested in
//! `b2bua::destination_allowlist::tests`; this pins the *wiring* end-to-end: a route to
//! a host that is neither an IP literal nor in the worker's allow-list must be
//! answered with a `503` and torn down *before* any b-leg INVITE is sent — so a
//! bogus host (a typo, a `.svc.cluster.local` name with no live pod) never reaches
//! the send path and blocks on `getaddrinfo`/`EAI_AGAIN`.
//!
//! The harness baseline keeps the production default allow-list
//! (`[".svc.cluster.local"]`), so routing to `kindlab` triggers the gate. This
//! mirrors `failure.rs`'s `decision_reject_answers_caller_directly`: alice gets a
//! direct final response, no b-leg is created, and exactly one CDR with a `Reject`
//! event is written. Pure paused-clock; `h.finish()` runs the RFC post-call audit
//! over the relayed `503`.
//!
//! The second test pins the load-bearing property of the gate (the one the TS
//! `apply-route-admission-reject.test.ts` proves with `Effect.die` stubs): the
//! admission reject lands BEFORE the limiter is touched. The route carries a
//! `call_limiter` entry, yet admission must short-circuit so the limiter's
//! `admit()` is never invoked — no hold is allocated for a doomed target. A
//! recording spy limiter (the Rust analogue of the TS die-on-call stub) records
//! whether `admit` ran and the test asserts it did not.

use std::sync::Arc;

use b2bua::decision::{test_adapter::route_to, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::LimiterEntry;
use b2bua_harness::limiter::doubles::{fail_open, spy};
use b2bua_harness::{settle_until, B2buaSut};
use call::CdrEventType;
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=a 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

#[tokio::test]
async fn non_allow_listed_b_leg_host_is_503ed_before_any_invite_is_sent() {
    let h = Harness::with_transit_delay("b2bua-admission-reject", 0);
    let alice = h.agent("alice", "127.0.0.1:5063").await;
    // `bob` exists so a leaked b-leg INVITE would have somewhere to land and the
    // assertion "no INVITE reached bob" is meaningful — but the route points at
    // the bogus host `kindlab`, NOT bob's address.
    let bob = h.agent("bob", "127.0.0.1:5073").await;
    let b2bua = B2buaSut::route_all_to("kindlab", 5073).start(&h, "b2bua", "127.0.0.1:5083").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;

    // The admission gate answers the caller directly with 503, before any b-leg.
    call.expect(503).await;

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the admission-rejected call");
    assert!(
        cdrs[0].b_legs.is_empty(),
        "no b-leg created on admission reject (host never reached the send path)"
    );
    let kinds: Vec<CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(
        kinds.contains(&CdrEventType::Reject),
        "admission reject must record a Reject CDR event: {kinds:?}"
    );

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _r = h.finish().await;
}

#[tokio::test]
async fn admission_reject_short_circuits_before_the_limiter_is_touched() {
    let h = Harness::with_transit_delay("b2bua-admission-reject-no-limiter", 0);
    let alice = h.agent("alice", "127.0.0.1:5063").await;
    let bob = h.agent("bob", "127.0.0.1:5073").await;

    // Route every call to the bogus host `kindlab` AND attach a `call_limiter`
    // entry — so a route that DID acquire a hold would invoke `admit`. The gate
    // must reject before that happens (admission runs ahead of the limiter loop
    // in `apply_route`).
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to("kindlab", 5073);
                r.call_limiter.push(LimiterEntry { id: "global".into(), limit: 100 });
                NewCallResponse::Route(r)
            })
            .build(),
    );
    // Were admission ever to reach the limiter, it would fail open.
    let spy = spy(fail_open());
    let b2bua =
        B2buaSut::builder(decision).limiter(spy.clone()).start(&h, "b2bua", "127.0.0.1:5083").await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    call.expect(503).await;

    settle_until(|| !b2bua.cdr_records().is_empty()).await;

    // The load-bearing assertion: the limiter was never consulted on the reject
    // path (no hold allocated for a target that never reaches the send path).
    assert!(
        spy.admits().is_empty(),
        "admission reject must short-circuit BEFORE the limiter's admit() — even \
         though the route carried a call_limiter entry"
    );
    assert!(
        !spy.touched(),
        "no limiter method (admit/release/refresh) runs on an admission reject"
    );

    // And it is still the admission reject (not some other 503 path): no b-leg,
    // a Reject CDR.
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the admission-rejected call");
    assert!(cdrs[0].b_legs.is_empty(), "no b-leg created on admission reject");
    let kinds: Vec<CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(
        kinds.contains(&CdrEventType::Reject),
        "admission reject must record a Reject CDR event: {kinds:?}"
    );

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _r = h.finish().await;
}
