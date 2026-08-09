//! What the failing callee said travels onto the a-facing final the failover
//! path mints (RFC 3261 §16.6, ADR-0017 X2).
//!
//! On the failover path the caller's final is decision-authored
//! (`RespondToALeg`) or re-synthesized (`RelayFailureToALeg`) — both are the
//! stack's own messages, so nothing the refusing peer stated reaches the
//! caller unless these mint points carry it. What is lost otherwise is exactly
//! what a refusal is diagnosed and billed on: the `Warning` behind the status
//! code, the charging correlation, the vendor's own annotation of why.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use b2bua_harness::{settle_until, B2buaScene, B2buaSut, BOB_PORT};
use call_limiter::{LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use sip_clock::Clock;
use sip_message::header::HeaderName;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// The second attempt's callee, dialed after bob's refusal reroutes the plan.
const CAROL_PORT: u16 = 5071;

/// One route to bob; on its failure the plan applies `on_exhausted`.
fn plan(on_exhausted: serde_json::Value) -> String {
    serde_json::json!({
        "action": "route",
        "routes": [{"destination": {"host": "127.0.0.1", "port": BOB_PORT}}],
        "on_exhausted": on_exhausted,
    })
    .to_string()
}

/// bob, then carol with a ring deadline; on exhaustion the plan applies
/// `on_exhausted`. The shape a superseded attempt needs: attempt 1 draws a
/// final, attempt 2 does not.
fn reroute_plan(carol_no_answer_sec: i64, on_exhausted: serde_json::Value) -> String {
    serde_json::json!({
        "routes": [
            {"destination": {"host": "127.0.0.1", "port": BOB_PORT}},
            {
                "destination": {"host": "127.0.0.1", "port": CAROL_PORT},
                "no_answer_timeout_sec": carol_no_answer_sec,
            },
        ],
        "on_exhausted": on_exhausted,
    })
    .to_string()
}

async fn plan_scene(name: &str) -> B2buaScene {
    B2buaScene::with_b2bua(name, |_bob_port| {
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::numbering_plan()))
    })
    .await
}

/// The callee refuses with the full diagnostic set; the decision authors its
/// own final (a different status — proof the message is decision-built, not
/// relayed). Every relayable header the callee stated rides that final, plus
/// the decision's own `Reason`.
#[tokio::test(start_paused = true)]
async fn a_failing_finals_headers_ride_the_decision_authored_final() {
    let s = plan_scene("failure-hdr-decision-final").await;
    let plan = plan(serde_json::json!({
        "action": "reject", "code": 480, "reason": "Temporarily Unavailable",
        "update_headers": {"Reason": "Q.850;cause=34"}
    }));

    let mut call =
        s.alice.invite(&s.bob).with_sdp(OFFER).with_header("X-Api-Call", &plan).through(s.b2bua.addr).send().await;
    s.bob
        .receive("INVITE")
        .await
        .respond(486, "Busy Here")
        .with_header("Warning", "399 gw.example \"Refused by ISUP\"")
        .with_header("P-Charging-Vector", "icid-value=\"cv-bleg-1\";orig-ioi=op.example;term-ioi=term.example")
        .with_header("P-Vendor-Thing", "annotation")
        .with_header("Allow", "INVITE, ACK, CANCEL, BYE")
        .await;
    s.bob.receive("ACK").await; // the b2bua completes bob's reject txn (§17.1.1.3)

    let resp = call.expect(480).await;
    let raw = |name: &str| -> Vec<String> {
        resp.raw(HeaderName::from(name)).map(str::to_string).collect()
    };
    assert_eq!(raw("Warning"), ["399 gw.example \"Refused by ISUP\""], "the callee's Warning reaches the caller");
    assert_eq!(
        raw("P-Charging-Vector"),
        ["icid-value=\"cv-bleg-1\";orig-ioi=op.example;term-ioi=term.example"],
        "the charging correlation survives the refusal"
    );
    assert_eq!(raw("P-Vendor-Thing"), ["annotation"], "the vendor annotation reaches the caller");
    assert_eq!(raw("Allow"), ["INVITE, ACK, CANCEL, BYE"], "the refusing party's advertisement reaches the caller");
    assert_eq!(raw("Reason"), ["Q.850;cause=34"], "the decision's own statement rides too");

    settle_until(|| s.b2bua.active_calls() == 0).await;
    s.b2bua.assert_fully_reaped();
    let _report = s.finish().await;
}

/// A `header_updates` entry naming a relayed header owns that name: a set
/// value replaces the callee's, a removal keeps it off the final. Names the
/// decision does not state still travel.
#[tokio::test(start_paused = true)]
async fn a_decision_header_update_owns_the_name_it_states() {
    let s = plan_scene("failure-hdr-decision-wins").await;
    let plan = plan(serde_json::json!({
        "action": "reject", "code": 486, "reason": "Busy Here",
        "update_headers": {
            "Warning": "399 plan.example \"authored by the plan\"",
            "P-Charging-Vector": null
        }
    }));

    let mut call =
        s.alice.invite(&s.bob).with_sdp(OFFER).with_header("X-Api-Call", &plan).through(s.b2bua.addr).send().await;
    s.bob
        .receive("INVITE")
        .await
        .respond(486, "Busy Here")
        .with_header("Warning", "399 gw.example \"the peers value\"")
        .with_header("P-Charging-Vector", "icid-value=\"cv-bleg-2\"")
        .with_header("P-Vendor-Thing", "annotation")
        .await;
    s.bob.receive("ACK").await;

    let resp = call.expect(486).await;
    let raw = |name: &str| -> Vec<String> {
        resp.raw(HeaderName::from(name)).map(str::to_string).collect()
    };
    assert_eq!(
        raw("Warning"),
        ["399 plan.example \"authored by the plan\""],
        "the decision's set value stands alone — the relayed one folds under it"
    );
    assert_eq!(raw("P-Charging-Vector"), Vec::<String>::new(), "the decision's removal wins over the relayed value");
    assert_eq!(raw("P-Vendor-Thing"), ["annotation"], "a name the decision does not state still travels");

    settle_until(|| s.b2bua.active_calls() == 0).await;
    s.b2bua.assert_fully_reaped();
    let _report = s.finish().await;
}

/// The exhaustion `relay` treatment re-synthesizes the b-leg failure on the
/// a-leg txn (`RelayFailureToALeg`) — the synthesized final restates what the
/// callee's own final carried.
#[tokio::test(start_paused = true)]
async fn the_resynthesized_relay_final_restates_the_callees_headers() {
    let s = plan_scene("failure-hdr-relay-final").await;
    let plan = plan(serde_json::json!({"action": "relay"}));

    let mut call =
        s.alice.invite(&s.bob).with_sdp(OFFER).with_header("X-Api-Call", &plan).through(s.b2bua.addr).send().await;
    s.bob
        .receive("INVITE")
        .await
        .respond(486, "Busy Here")
        .with_header("Warning", "399 gw.example \"Refused by ISUP\"")
        .with_header("P-Charging-Vector", "icid-value=\"cv-bleg-3\"")
        .with_header("Allow", "INVITE, ACK, CANCEL, BYE")
        .await;
    s.bob.receive("ACK").await;

    let resp = call.expect(486).await;
    let raw = |name: &str| -> Vec<String> {
        resp.raw(HeaderName::from(name)).map(str::to_string).collect()
    };
    assert_eq!(raw("Warning"), ["399 gw.example \"Refused by ISUP\""]);
    assert_eq!(raw("P-Charging-Vector"), ["icid-value=\"cv-bleg-3\""]);
    assert_eq!(raw("Allow"), ["INVITE, ACK, CANCEL, BYE"]);

    settle_until(|| s.b2bua.active_calls() == 0).await;
    s.b2bua.assert_fully_reaped();
    let _report = s.finish().await;
}

/// RFC 3325 §7 / RFC 3323 §5.3: the failing final asks for privacy over its
/// identity, so the assertion stays behind while the instruction — and every
/// other relayable header — still travels.
#[tokio::test(start_paused = true)]
async fn privacy_id_withholds_the_identity_the_failing_final_conceals() {
    let s = plan_scene("failure-hdr-privacy").await;
    let plan = plan(serde_json::json!({"action": "relay"}));

    let mut call =
        s.alice.invite(&s.bob).with_sdp(OFFER).with_header("X-Api-Call", &plan).through(s.b2bua.addr).send().await;
    s.bob
        .receive("INVITE")
        .await
        .respond(486, "Busy Here")
        .with_header("Privacy", "id")
        .with_header("P-Asserted-Identity", "<sip:+15550001@op.example>")
        .with_header("P-Vendor-Thing", "annotation")
        .await;
    s.bob.receive("ACK").await;

    let resp = call.expect(486).await;
    let raw = |name: &str| -> Vec<String> {
        resp.raw(HeaderName::from(name)).map(str::to_string).collect()
    };
    assert_eq!(
        raw("P-Asserted-Identity"),
        Vec::<String>::new(),
        "the identity a Privacy: id conceals never crosses the leg"
    );
    assert_eq!(raw("Privacy"), ["id"], "the privacy instruction itself travels");
    assert_eq!(raw("P-Vendor-Thing"), ["annotation"], "privacy withholds only the assertion");

    settle_until(|| s.b2bua.active_calls() == 0).await;
    s.b2bua.assert_fully_reaped();
    let _report = s.finish().await;
}

/// A superseded attempt's headers leave with it: bob refuses with the full
/// diagnostic set, the plan reroutes, and the second attempt draws NO final at
/// all. The final the decision then authors speaks for a peer that never
/// answered — nothing bob said may ride it.
#[tokio::test(start_paused = true)]
async fn a_superseded_attempts_headers_do_not_answer_a_later_no_answer() {
    let s = plan_scene("failure-hdr-superseded-no-answer").await;
    let carol = s.h.agent("carol", &format!("127.0.0.1:{CAROL_PORT}")).await;
    let plan = reroute_plan(
        5,
        serde_json::json!({"action": "reject", "code": 480, "reason": "Temporarily Unavailable"}),
    );

    let mut call =
        s.alice.invite(&s.bob).with_sdp(OFFER).with_header("X-Api-Call", &plan).through(s.b2bua.addr).send().await;
    s.bob
        .receive("INVITE")
        .await
        .respond(486, "Busy Here")
        .with_header("Warning", "399 gw.example \"Refused by ISUP\"")
        .with_header("Retry-After", "300")
        .with_header("P-Charging-Vector", "icid-value=\"cv-attempt-1\"")
        .await;
    s.bob.receive("ACK").await;

    // Attempt 2 rings and never answers; its ring deadline exhausts the plan.
    let mut carol_uas = carol.receive("INVITE").await;
    carol_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    s.h.advance(Duration::from_secs(6)).await;
    let mut cancel = carol.receive("CANCEL").await;
    cancel.respond(200, "OK").await;
    carol_uas.respond(487, "Request Terminated").await;
    carol.receive("ACK").await; // the b2bua completes carol's 487 txn (§17.1.1.3)

    let resp = call.expect(480).await;
    let raw = |name: &str| -> Vec<String> {
        resp.raw(HeaderName::from(name)).map(str::to_string).collect()
    };
    assert_eq!(raw("Warning"), Vec::<String>::new(), "the superseded attempt's Warning does not diagnose this failure");
    assert_eq!(raw("Retry-After"), Vec::<String>::new(), "a stale Retry-After names a peer that was never contacted");
    assert_eq!(
        raw("P-Charging-Vector"),
        Vec::<String>::new(),
        "no charging correlation for a leg that produced no final"
    );

    settle_until(|| s.b2bua.active_calls() == 0).await;
    s.b2bua.assert_fully_reaped();
    let _report = s.finish().await;
}

/// A plan-authored redirect is a new instruction, not a relayed refusal: RFC
/// 3261 §20.33 gives `Retry-After` a per-status meaning (on a 3xx it declares
/// the redirect Contact's validity, not when the refusing callee frees up),
/// so nothing the refusing peer stated rides the 302.
#[tokio::test(start_paused = true)]
async fn a_plan_authored_redirect_carries_none_of_the_refusals_headers() {
    let s = plan_scene("failure-hdr-redirect").await;
    let plan = plan(serde_json::json!({
        "action": "redirect", "code": 302, "reason": "Moved Temporarily",
        "contacts": [{"uri": "sip:backup@10.9.9.9:5062"}]
    }));

    let mut call =
        s.alice.invite(&s.bob).with_sdp(OFFER).with_header("X-Api-Call", &plan).through(s.b2bua.addr).send().await;
    s.bob
        .receive("INVITE")
        .await
        .respond(486, "Busy Here")
        .with_header("Warning", "399 gw.example \"Refused by ISUP\"")
        .with_header("Retry-After", "3600")
        .with_header("P-Charging-Vector", "icid-value=\"cv-bleg-4\"")
        .await;
    s.bob.receive("ACK").await;

    let resp = call.expect(302).await;
    let raw = |name: &str| -> Vec<String> {
        resp.raw(HeaderName::from(name)).map(str::to_string).collect()
    };
    let contacts = raw("Contact");
    assert_eq!(contacts.len(), 1, "the redirect carries its authored Contact");
    assert!(
        contacts[0].contains("sip:backup@10.9.9.9:5062"),
        "the Contact names the plan's target: {contacts:?}"
    );
    assert_eq!(
        raw("Retry-After"),
        Vec::<String>::new(),
        "a relayed Retry-After would redeclare the Contact's validity (RFC 3261 §20.33)"
    );
    assert_eq!(raw("Warning"), Vec::<String>::new(), "the redirect explains no refusal");
    assert_eq!(
        raw("P-Charging-Vector"),
        Vec::<String>::new(),
        "a new instruction correlates no peer's charging"
    );

    settle_until(|| s.b2bua.active_calls() == 0).await;
    s.b2bua.assert_fully_reaped();
    let _report = s.finish().await;
}

/// A resolution reached through the router's `call_limiter` re-consult answers
/// the limiter refusal, not the failed peer's final: bob refuses with the full
/// diagnostic set, the plan fails over to a hop whose limiter entry is already
/// over cap, and the plan's exhaustion reject reaches alice — carrying nothing
/// bob said about a gateway this failure is not about.
#[tokio::test(start_paused = true)]
async fn a_capacity_refusal_carries_none_of_the_failed_peers_headers() {
    let laddr: SocketAddr = "10.0.0.1:8080".parse().unwrap();
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(WindowStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let _lh: Box<dyn HttpServerHandle> = http.serve(laddr, server).await.unwrap();
    let limiter: Arc<dyn CallLimiter> = Arc::new(HttpCallLimiter::new(
        Arc::new(http.clone()),
        laddr,
        Duration::from_millis(150),
    ));
    let s = B2buaScene::with_b2bua("failure-hdr-limiter-refusal", move |_bob_port| {
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::numbering_plan())).limiter(limiter)
    })
    .await;
    // The failover hop's limiter entry admits nothing, so its refusal — not a
    // peer final — is what exhausts the plan (carol is never dialed).
    let plan = serde_json::json!({
        "routes": [
            {"destination": {"host": "127.0.0.1", "port": BOB_PORT}},
            {
                "destination": {"host": "127.0.0.1", "port": CAROL_PORT},
                "call_limiter": [{"id": "capacity-trunk", "limit": 0}],
            },
        ],
        "on_exhausted": {"action": "reject", "code": 480, "reason": "Temporarily Unavailable"},
    })
    .to_string();

    let mut call =
        s.alice.invite(&s.bob).with_sdp(OFFER).with_header("X-Api-Call", &plan).through(s.b2bua.addr).send().await;
    s.bob
        .receive("INVITE")
        .await
        .respond(486, "Busy Here")
        .with_header("Warning", "399 gw.example \"Refused by ISUP\"")
        .with_header("Retry-After", "300")
        .with_header("P-Charging-Vector", "icid-value=\"cv-bleg-5\"")
        .await;
    s.bob.receive("ACK").await;

    let resp = call.expect(480).await;
    let raw = |name: &str| -> Vec<String> {
        resp.raw(HeaderName::from(name)).map(str::to_string).collect()
    };
    assert_eq!(
        raw("Warning"),
        Vec::<String>::new(),
        "bob's Warning does not explain the stack's own capacity statement"
    );
    assert_eq!(
        raw("Retry-After"),
        Vec::<String>::new(),
        "a relayed Retry-After would speak for a gateway this failure is not about"
    );
    assert_eq!(
        raw("P-Charging-Vector"),
        Vec::<String>::new(),
        "a capacity refusal correlates no peer's charging"
    );

    settle_until(|| s.b2bua.active_calls() == 0).await;
    s.b2bua.assert_fully_reaped();
    let _report = s.finish().await;
}

/// The a-leg setup deadline answers the CALLER's wait, not the b-leg's
/// refusal: the 408 it mints is the stack's own statement and carries none of
/// the failing peer's diagnostics, even with a failure round trip in flight.
#[tokio::test(start_paused = true)]
async fn the_setup_deadline_final_speaks_only_for_itself() {
    let s = B2buaScene::with_b2bua("failure-hdr-setup-deadline", |_bob_port| {
        B2buaSut::builder(Arc::new(ScriptedDecisionEngine::numbering_plan()))
            .tune(|c| c.setup_timeout_sec = 10)
    })
    .await;
    let carol = s.h.agent("carol", &format!("127.0.0.1:{CAROL_PORT}")).await;
    // Carol's ring deadline sits past the setup deadline, so the 408 — not the
    // plan — is what answers alice.
    let plan = reroute_plan(60, serde_json::json!({"action": "relay"}));

    let mut call =
        s.alice.invite(&s.bob).with_sdp(OFFER).with_header("X-Api-Call", &plan).through(s.b2bua.addr).send().await;
    s.bob
        .receive("INVITE")
        .await
        .respond(486, "Busy Here")
        .with_header("Warning", "399 gw.example \"Refused by ISUP\"")
        .with_header("P-Charging-Vector", "icid-value=\"cv-attempt-1\"")
        .await;
    s.bob.receive("ACK").await;

    let mut carol_uas = carol.receive("INVITE").await;
    carol_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    s.h.advance(Duration::from_secs(11)).await;
    let mut cancel = carol.receive("CANCEL").await;
    cancel.respond(200, "OK").await;
    carol_uas.respond(487, "Request Terminated").await;
    carol.receive("ACK").await; // the b2bua completes carol's 487 txn (§17.1.1.3)

    let resp = call.expect(408).await;
    let raw = |name: &str| -> Vec<String> {
        resp.raw(HeaderName::from(name)).map(str::to_string).collect()
    };
    assert_eq!(raw("Warning"), Vec::<String>::new(), "the refusing peer's Warning does not explain a setup deadline");
    assert_eq!(
        raw("P-Charging-Vector"),
        Vec::<String>::new(),
        "the timed-out final correlates no peer's charging"
    );

    settle_until(|| s.b2bua.active_calls() == 0).await;
    s.b2bua.assert_fully_reaped();
    let _report = s.finish().await;
}
