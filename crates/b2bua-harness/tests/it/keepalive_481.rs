//! Keepalive where one peer answers 481 (port of `tests/scenarios/keepalive-481.ts`).
//!
//! Long call; the keepalive timer fires and the B2BUA sends an in-dialog OPTIONS
//! to *both* legs. Alice replies 200 OK; Bob replies 481 "Call/Transaction Does
//! Not Exist". RFC 3261 §12.2.1.2: a 481 to an in-dialog request ends the
//! dialog, and for an INVITE-initiated dialog that is a BYE — so `handle-481`
//! records the failure and `begin-termination` BYEs BOTH legs, the 481 peer
//! included. Bob, holding no dialog, answers that BYE 481 too; any final to our
//! BYE resolves the leg (RFC 3261 §15.1.1, `resolve-bye-response`).
//!
//! Exercises `keepalive` + `absorb-options-200` + `handle-481`. The source backend
//! used a 15-min interval; the Rust default keepalive interval is 30 s, so we
//! advance in 30 s steps — the behaviour is identical.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua_harness::{settle_until, B2buaScene};
use call::CdrEventType;
use scenario_harness::RunReport;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

/// The Rust default keepalive interval (`KeepaliveActivation.interval_sec`).
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Distinct BYE requests the B2BUA put on the wire toward `to` (retransmits
/// of one BYE share a branch and count once).
fn byes_to(report: &RunReport, to: SocketAddr) -> usize {
    let mut branches: Vec<String> = report
        .entries()
        .iter()
        .filter(|e| e.to == to)
        .filter_map(|e| match CustomParser::new().parse(&e.raw) {
            Ok(SipMessage::Request(r)) if r.method() == "BYE" => {
                r.via().first().branch().map(|b| b.to_string())
            }
            _ => None,
        })
        .collect();
    branches.sort();
    branches.dedup();
    branches.len()
}

#[tokio::test(start_paused = true)]
async fn bob_481_on_options_byes_both_peers() {
    let s = B2buaScene::new("b2bua-keepalive-481").await;

    // ── Call setup ───────────────────────────────────────────────────────────
    let _dialog = s.establish().await;

    // ── Keepalive fires: alice 200, bob 481 ──────────────────────────────────
    s.h.advance(KEEPALIVE_INTERVAL).await;
    s.alice.receive("OPTIONS").await.respond(200, "OK").await;
    s.bob.receive("OPTIONS").await.respond(481, "Call/Transaction Does Not Exist").await;

    // ── handle-481 → begin-termination → BYE to BOTH peers ───────────────────
    s.alice.receive("BYE").await.respond(200, "OK").await;
    // Bob holds no dialog: his answer to the BYE is a 481 as well, and that
    // final resolves his leg like a 200 would.
    s.bob.receive("BYE").await.respond(481, "Call/Transaction Does Not Exist").await;

    // The CDR records the 481-driven teardown.
    settle_until(|| !s.b2bua.cdr_records().is_empty()).await;
    let cdrs = s.b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the 481-terminated call");
    let kinds: Vec<CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(kinds.contains(&CdrEventType::Bye), "bye event from 481 handling: {kinds:?}");
    s.b2bua.assert_fully_reaped();

    // ── One BYE to each peer ─────────────────────────────────────────────────
    let (alice_addr, bob_addr) = (s.alice.addr(), s.bob.addr());
    let report = s.finish().await;
    assert_eq!(byes_to(&report, alice_addr), 1, "one BYE to the healthy peer");
    assert_eq!(byes_to(&report, bob_addr), 1, "one BYE to the 481 peer (RFC 3261 §12.2.1.2)");
}
