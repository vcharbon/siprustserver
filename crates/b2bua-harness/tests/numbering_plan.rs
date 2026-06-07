//! Advanced numbering-plan services driven entirely by the `X-Api-Call` plan
//! header (ADR-0017). Each scenario injects, from alice, one JSON blob that
//! conveys *how to treat the call* and asserts the B2BUA executes it:
//!
//!   1. route + rewrite every field (From/To numbers, full R-URI, PAI, PANI),
//!   2. route → reroute to the second destination on failure,
//!   3. direct rejection carrying a `Reason` header,
//!   4. direct 302 redirect carrying an ordered Contact list,
//!   5. reroute exhaustion → adapter-chosen 302 redirect (treatment-at-any-hop).
//!
//! The whole plan rides one header; the remaining reroute list is round-tripped
//! through the opaque `callback_context` between hops (see `test_adapter`).

use std::path::Path;
use std::sync::Arc;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::B2buaSut;
use scenario_harness::{Harness, RunReport};
use sip_message::message_helpers::{get_header, get_headers};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

fn plan_engine() -> Arc<ScriptedDecisionEngine> {
    Arc::new(ScriptedDecisionEngine::numbering_plan())
}

/// Finish the run and render the SIP call-flow artifacts (`<name>.html` +
/// `.svg` + `.global.txt`) under `target/seq-reports/numbering-plan/`, so each
/// scenario has a visual sequence diagram alongside the assertions.
async fn finish_with_report(h: Harness) {
    let report: RunReport = h.finish().await;
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/seq-reports/numbering-plan");
    let paths = scenario_harness::report::write_all(&report, &dir).expect("write report");
    if let Some(html) = paths.iter().find(|p| p.extension().is_some_and(|e| e == "html")) {
        eprintln!("numbering-plan report: {}", html.display());
    }
}

// ── 1. Route + rewrite all fields ───────────────────────────────────────────

#[tokio::test]
async fn route_rewrites_from_to_ruri_pai_and_pani() {
    let h = Harness::with_transit_delay("plan-rewrite", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::start(&h, "b2bua", "127.0.0.1:5080", plan_engine()).await;

    // Wire destination is bob; the R-URI/From/To numbers are rewritten freely.
    let plan = serde_json::json!({
        "action": "route",
        "destination": {"host": "127.0.0.1", "port": 5070},
        "new_ruri": "sip:+18001234@carrier.example",
        "new_from": "sip:+15551000@trunk.example",
        "new_to": "sip:+19005678@carrier.example",
        "update_headers": {
            "P-Asserted-Identity": "sip:+15551000@trunk.example",
            "P-Access-Network-Info": "3GPP-E-UTRAN-FDD; utran-cell-id-3gpp=1234"
        }
    })
    .to_string();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .through(b2bua.addr)
        .send()
        .await;

    let mut bob_uas = bob.receive("INVITE").await;
    let req = bob_uas.request();
    assert_eq!(req.uri, "sip:+18001234@carrier.example", "R-URI rewritten");
    assert!(
        get_header(&req.headers, "from").unwrap_or("").contains("+15551000@trunk.example"),
        "From number rewritten: {:?}",
        get_header(&req.headers, "from")
    );
    assert!(
        get_header(&req.headers, "to").unwrap_or("").contains("+19005678@carrier.example"),
        "To number rewritten: {:?}",
        get_header(&req.headers, "to")
    );
    assert_eq!(
        get_header(&req.headers, "p-asserted-identity"),
        Some("sip:+15551000@trunk.example"),
        "PAI added"
    );
    assert_eq!(
        get_header(&req.headers, "p-access-network-info"),
        Some("3GPP-E-UTRAN-FDD; utran-cell-id-3gpp=1234"),
        "PANI added"
    );

    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    call.ack().await;
    bob.receive("ACK").await;
    finish_with_report(h).await;
}

// ── 2. Route → reroute to the second destination on failure ─────────────────

#[tokio::test]
async fn reroutes_to_second_destination_on_failure() {
    let h = Harness::with_transit_delay("plan-reroute", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let carol = h.agent("carol", "127.0.0.1:5070").await; // first attempt — fails
    let bob = h.agent("bob", "127.0.0.1:5071").await; // second attempt — answers
    let b2bua = B2buaSut::start(&h, "b2bua", "127.0.0.1:5080", plan_engine()).await;

    let plan = serde_json::json!({
        "action": "route",
        "routes": [
            {"destination": {"host": "127.0.0.1", "port": 5070}, "new_to": "sip:+1@carol"},
            {"destination": {"host": "127.0.0.1", "port": 5071}, "new_to": "sip:+1@bob"}
        ],
        "on_exhausted": {"action": "relay"}
    })
    .to_string();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .through(b2bua.addr)
        .send()
        .await;

    // First destination rejects → reroute to the second.
    let mut carol_uas = carol.receive("INVITE").await;
    assert!(carol_uas.request().headers.iter().any(|h| h
        .name
        .eq_ignore_ascii_case("to")
        && h.value.contains("+1@carol")));
    carol_uas.respond(503, "Service Unavailable").await;

    // Second destination receives the rerouted INVITE and answers.
    let mut bob_uas = bob.receive("INVITE").await;
    assert!(bob_uas.request().headers.iter().any(|h| h
        .name
        .eq_ignore_ascii_case("to")
        && h.value.contains("+1@bob")));
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    call.ack().await;
    bob.receive("ACK").await;
    finish_with_report(h).await;
}

// ── 3. Direct rejection carrying a Reason header ─────────────────────────────

#[tokio::test]
async fn direct_reject_carries_reason_header() {
    let h = Harness::with_transit_delay("plan-reject", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::start(&h, "b2bua", "127.0.0.1:5080", plan_engine()).await;

    let plan = serde_json::json!({
        "action": "reject",
        "code": 603,
        "reason": "Declined",
        "update_headers": {"Reason": "Q.850;cause=21;text=\"call rejected\""}
    })
    .to_string();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .through(b2bua.addr)
        .send()
        .await;

    let resp = call.expect(603).await;
    assert_eq!(resp.status, 603);
    assert_eq!(
        get_header(&resp.headers, "reason"),
        Some("Q.850;cause=21;text=\"call rejected\""),
        "Reason header relayed on the rejection"
    );
    finish_with_report(h).await;
}

// ── 4. Direct 302 redirect carrying an ordered Contact list ──────────────────

#[tokio::test]
async fn direct_302_redirect_carries_contact_list() {
    let h = Harness::with_transit_delay("plan-redirect", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::start(&h, "b2bua", "127.0.0.1:5080", plan_engine()).await;

    let plan = serde_json::json!({
        "action": "redirect",
        "code": 302,
        "reason": "Moved Temporarily",
        "contacts": [
            {"uri": "sip:primary@alt1.example", "q": 1.0},
            {"uri": "sip:backup@alt2.example", "q": 0.5}
        ]
    })
    .to_string();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .through(b2bua.addr)
        .send()
        .await;

    let resp = call.expect(302).await;
    assert_eq!(resp.status, 302);
    let contacts = get_headers(&resp.headers, "contact");
    assert_eq!(contacts.len(), 2, "two Contact headers: {contacts:?}");
    assert!(contacts[0].contains("sip:primary@alt1.example") && contacts[0].contains("q=1"));
    assert!(contacts[1].contains("sip:backup@alt2.example") && contacts[1].contains("q=0.5"));
    finish_with_report(h).await;
}

// ── 5. Reroute exhaustion → adapter-chosen 302 redirect ──────────────────────

#[tokio::test]
async fn reroute_exhaustion_redirects_caller() {
    let h = Harness::with_transit_delay("plan-exhaust-redirect", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let carol = h.agent("carol", "127.0.0.1:5070").await; // only attempt — fails
    let bob = h.agent("bob", "127.0.0.1:5071").await; // never dialed
    let b2bua = B2buaSut::start(&h, "b2bua", "127.0.0.1:5080", plan_engine()).await;

    let plan = serde_json::json!({
        "action": "route",
        "routes": [{"destination": {"host": "127.0.0.1", "port": 5070}}],
        "on_exhausted": {
            "action": "redirect",
            "code": 302,
            "contacts": [{"uri": "sip:overflow@alt.example", "q": 1.0}]
        }
    })
    .to_string();

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("X-Api-Call", &plan)
        .through(b2bua.addr)
        .send()
        .await;

    carol.receive("INVITE").await.respond(503, "Service Unavailable").await;

    // List exhausted → the plan's on_exhausted 302 reaches alice.
    let resp = call.expect(302).await;
    assert_eq!(resp.status, 302);
    let contacts = get_headers(&resp.headers, "contact");
    assert!(
        contacts.iter().any(|c| c.contains("sip:overflow@alt.example")),
        "exhaustion redirect Contact present: {contacts:?}"
    );
    finish_with_report(h).await;
}
