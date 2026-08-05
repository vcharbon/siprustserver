//! What the failing callee said travels onto the a-facing final the failover
//! path mints (RFC 3261 §16.6, ADR-0017 X2).
//!
//! On the failover path the caller's final is decision-authored
//! (`RespondToALeg`) or re-synthesized (`RelayFailureToALeg`) — both are the
//! stack's own messages, so nothing the refusing peer stated reaches the
//! caller unless these mint points carry it. What is lost otherwise is exactly
//! what a refusal is diagnosed and billed on: the `Warning` behind the status
//! code, the charging correlation, the vendor's own annotation of why.

use std::sync::Arc;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{settle_until, B2buaScene, B2buaSut, BOB_PORT};
use sip_message::header::HeaderName;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// One route to bob; on its failure the plan applies `on_exhausted`.
fn plan(on_exhausted: serde_json::Value) -> String {
    serde_json::json!({
        "action": "route",
        "routes": [{"destination": {"host": "127.0.0.1", "port": BOB_PORT}}],
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
