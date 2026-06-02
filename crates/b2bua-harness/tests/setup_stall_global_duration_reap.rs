//! Regression: a call whose b-leg INVITE never reaches a final response must
//! still be reaped by the GlobalDuration cap.
//!
//! A call enters `Active` the instant its a-leg is built; `confirm-dialog` is
//! what arms GlobalDuration + Keepalive. So a call stuck mid-setup — the b-leg
//! INVITE rings forever / its 200 is lost / the UAS drops the INVITE under load
//! — has NEITHER timer. Its only other reaper is the NoAnswer ring timer, but
//! that is armed only when the route supplies `no_answer_timeout_sec`; the
//! scripted endurance adapter (`route_all_to`) supplies `None`. The result was
//! ~0.4% of calls reaching `Active` with an empty `call.timers`: no timer ever
//! fired and they leaked forever — surviving past even the 1h GlobalDuration cap
//! (it was never armed). Observed in k8s as ~1095 ESTABLISHED calls pinned flat
//! for >4 HOURS on a never-killed worker.
//!
//! The fix arms a GlobalDuration backstop at call creation (apply_route), so
//! every call carries the absolute duration cap. Here bob rings then goes
//! silent (no 200, no answer timeout); after the cap the existing `max-duration`
//! rule must reap the wedged call (active_calls -> 0).

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::B2buaSut;
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// A short max-duration, deliberately set *below* the b-leg INVITE client
/// transaction's Timer B (32 s). In production the leak is a stalled-setup call
/// whose Timer B timeout never reaches the rule engine — the txn was swept
/// (`TXN_MAX_AGE` 35 s) before a load-delayed Timer B fired, so `fire_timeout`
/// found no txn and emitted nothing (`sip-txn/layer.rs::fire_timeout` early
/// return). With GlobalDuration armed only at *answer*, such a call has no
/// reaper at all. By capping below Timer B we make GlobalDuration the sole
/// reaper here: the call MUST be torn down by the creation-time backstop, not by
/// the txn timeout — exactly the production safety net.
const MAX_DURATION_SEC: i64 = 20;
const MAX_DURATION: Duration = Duration::from_secs(MAX_DURATION_SEC as u64);

#[tokio::test(start_paused = true)]
async fn setup_stalled_call_is_reaped_by_global_duration() {
    let h = Harness::new("b2bua-setup-stall-reap");
    let alice = h.agent("alice", "127.0.0.1:5069").await;
    let bob = h.agent("bob", "127.0.0.1:5079").await;
    // Route every call to bob with NO `no_answer_timeout_sec` (the endurance
    // config) but a short `max_duration_sec`: the only reaper for a never-
    // answered call is the GlobalDuration backstop.
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut route = route_to("127.0.0.1", 5079);
                route.features.platform.max_duration_sec = MAX_DURATION_SEC;
                NewCallResponse::Route(route)
            })
            .build(),
    );
    let b2bua = B2buaSut::start(&h, "b2bua", "127.0.0.1:5089", decision).await;

    // ── Call setup that stalls: bob rings, then goes silent (no 200) ──────────
    let _call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    // Deliberately never send a final response — the call wedges `Active` with
    // its b-leg in `Early` and (before the fix) zero timers.

    assert_eq!(
        b2bua.metrics().creations_total() - b2bua.metrics().removals_total(),
        1,
        "exactly one active call wedged in setup",
    );

    // ── Safety net: at the GlobalDuration cap the wedged call must be reaped ───
    // GlobalDuration fires at the cap → `max-duration` rule begins termination
    // (CANCEL the early b-leg) → the call resolves to Terminated and is reaped.
    h.advance(MAX_DURATION + Duration::from_secs(1)).await;
    for _ in 0..50 {
        if b2bua.metrics().removals_total() == b2bua.metrics().creations_total() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let m = b2bua.metrics();
    assert_eq!(
        m.removals_total(),
        m.creations_total(),
        "a setup-stalled call must be reaped by the GlobalDuration cap (active_calls -> 0); \
         got creations={} removals={}",
        m.creations_total(),
        m.removals_total(),
    );

    let _report = h.finish().await;
}
