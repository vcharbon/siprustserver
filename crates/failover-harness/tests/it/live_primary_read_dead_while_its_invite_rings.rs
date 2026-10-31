//! **A live primary read dead while the INVITE it dialled rings.** The
//! primary of a new call asks its routing decision; the proxy misreads it
//! dead while it runs and tells the backup, which takes nothing over while
//! the replication stream from the primary stays open (ADR-0047 D1). The
//! decision returns and the primary dials the callee. The callee's 180, which
//! the proxy routes to the call's other owner while it reads the primary
//! dead, takes the call over at the backup, whose copy relays it and sends
//! the INVITE again on its record. The probe reads the primary alive again
//! while the callee rings: the leg's cookie names the call's primary, so the
//! callee's 200 reaches the primary, which never restarted, and the copy
//! stands down on the primary's write (ADR-0055 §8). One owner after the
//! flip, no call cut by evidence, one CDR.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use failover_harness::{FailoverHarness, WorkerHealth};

use crate::departure_support::{bring_up_deciding, placed, CountedDecision};
use crate::inflight_resend::{settle_to_one_cdr, survivor_of};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// How long the routing decision takes: past the misread verdict.
const DECISION_DELAY: Duration = Duration::from_secs(1);

/// The SIP port of a worker, every incarnation alike.
fn port_of(ordinal: &str) -> u16 {
    if ordinal == "b1" {
        5091
    } else {
        5092
    }
}

#[tokio::test(start_paused = true)]
async fn a_live_primary_read_dead_while_its_invite_rings_owns_the_answer_after_the_flip() {
    let name = "live-primary-read-dead-while-its-invite-rings";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let route = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let (decision, _consults) = CountedDecision::counting(route, DECISION_DELAY);
    let (proxy, w_b1, w_b2) = bring_up_deciding(&mut fh, decision).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    let primary = if pri_ord == "b1" { &w_b1 } else { &w_b2 };
    let copy = survivor_of(&pri_ord, &w_b1, &w_b2);
    let bak_ord = if pri_ord == "b1" { "b2" } else { "b1" };

    fh.mark("proxy", None, "misread", "the probe reads the live primary dead");
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(200)).await;
    assert!(!copy.serves(&call_ref), "the verdict takes nothing over: the stream is open");

    let mut at_bob = bob.receive("INVITE").await;
    at_bob.respond(180, "Ringing").await;
    call.expect(180).await;
    fh.advance(Duration::from_millis(300)).await;
    assert!(copy.serves(&call_ref), "the 180 the proxy routes to the backup takes the call over");

    fh.mark("proxy", None, "flip", "the probe reads the primary alive again");
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_millis(500)).await;
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive_absorbing("ACK", &["INVITE"]).await;
    fh.advance(Duration::from_secs(1)).await;

    let proxy_addr = proxy.addr();
    let entries = fh.sip_entries();
    let from_workers = |line: &str| {
        let mut ports: Vec<u16> = entries
            .iter()
            .filter(|e| e.to == proxy_addr && [5091, 5092].contains(&e.from.port()))
            .filter(|e| e.start_line().starts_with(line))
            .map(|e| e.from.port())
            .collect();
        ports.dedup();
        ports
    };
    let invite_200s_to_copy = entries
        .iter()
        .filter(|e| e.from == proxy_addr && e.to.port() == port_of(bak_ord))
        .filter(|e| e.start_line().starts_with("SIP/2.0 200"))
        .filter(|e| String::from_utf8_lossy(&e.raw).contains(" INVITE\r\n"))
        .count();
    assert_eq!(invite_200s_to_copy, 0, "the callee's 200 reaches the primary after the flip");
    assert_eq!(
        from_workers("SIP/2.0 200"),
        [port_of(&pri_ord)],
        "the caller's 200 is sent by the primary alone"
    );
    assert_eq!(from_workers("ACK sip:"), [port_of(&pri_ord)], "the primary ACKs the callee");
    assert!(from_workers("CANCEL").is_empty(), "no copy CANCELs the answered callee");
    assert!(from_workers("BYE").is_empty(), "no node ends the call on evidence");
    let stood_down = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(5), async || {
            !copy.serves(&call_ref)
        })
        .await;
    assert!(stood_down, "the copy stands down: one owner after the flip");
    assert!(primary.serves(&call_ref), "the primary serves the call");

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(40)).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
