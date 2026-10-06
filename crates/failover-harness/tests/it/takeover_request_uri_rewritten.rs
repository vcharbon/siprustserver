//! An acting backup takes over a call from an in-dialog request whose
//! Request-URI an element on the path rewrote: the callee's BYE reaches the
//! survivor with no `callRef` of ours, so the call is found through the
//! replicated SIP index, and the BYE is the outgoing leg's.
//!
//! ```text
//!   alice ──▶ LB ──▶ primary ──▶ LB ──▶ rewriter ──▶ bob
//!                    ✗ crash
//!   bob BYE ──▶ rewriter (Request-URI replaced) ──▶ LB ──▶ backup
//! ```
//!
//! The primary reboots within its budget and reclaims the deferred terminal,
//! so the call ends with its one CDR.

use std::net::SocketAddr;
use std::time::Duration;

use failover_harness::{
    assert_call_fully_released, total_cdrs_for, FailoverHarness, ProxySut, ReplicatedB2buaSut,
    WorkerHealth,
};
use sip_message::header::Uri;
use sip_message::{SipRequest, SipStr};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const REWRITER: &str = "127.0.0.1:5075";
const LB: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";
/// What the rewriter aims every in-dialog request at: no user, no parameter
/// of the B2BUA's Contact.
const REWRITTEN: &str = "sip:b2bua.invalid";

fn addr(a: &str) -> SocketAddr {
    a.parse().unwrap()
}

/// The primary the load balancer's cookie names, on whichever recorded route
/// carries it (the rewriter's own sits on top).
fn primary_of(invite: &SipRequest) -> String {
    let recorded = invite.record_route_set().expect("readable Record-Route");
    let primary = recorded
        .iter()
        .find_map(|rr| rr.uri().param("w_pri").and_then(|v| v.as_str()).map(str::to_string));
    primary.expect("the load balancer recorded its cookie")
}

fn pair<'a>(
    w_b1: &'a mut ReplicatedB2buaSut,
    w_b2: &'a mut ReplicatedB2buaSut,
    primary_ord: &str,
) -> (&'a mut ReplicatedB2buaSut, &'a mut ReplicatedB2buaSut) {
    if primary_ord == "b1" {
        (w_b1, w_b2)
    } else {
        (w_b2, w_b1)
    }
}

#[tokio::test(start_paused = true)]
async fn a_callee_bye_without_our_request_uri_reaches_the_backup_on_the_outgoing_leg() {
    let mut fh = FailoverHarness::new("takeover-request-uri-rewritten", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let rewriter = fh.scripted_proxy("rewriter", REWRITER).await;
    let proxy: ProxySut = fh.spawn_proxy(LB, &[("b1", addr(B1)), ("b2", addr(B2))]).await;
    let rewriter_at = ("127.0.0.1", 5075);
    let lb_at = ("127.0.0.1", 5080);
    let mut w_b1 = fh.spawn_worker("b1", "b1", B1, &["b2"], rewriter_at, lb_at).await;
    let mut w_b2 = fh.spawn_worker("b2", "b2", B2, &["b1"], rewriter_at, lb_at).await;
    fh.advance(Duration::from_millis(500)).await;

    // INVITE → 200 → ACK, every callee-side message through the rewriter.
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    rewriter.forward_request(addr(BOB)).await;
    let mut uas = bob.receive("INVITE").await;
    let primary_ord = primary_of(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    rewriter.forward_response(addr(LB)).await;
    call.expect(200).await;
    let _alice_dialog = call.ack().await;
    rewriter.forward_request(addr(BOB)).await;
    bob.receive("ACK").await;
    let mut bob_dialog = uas.dialog();

    fh.advance(Duration::from_millis(500)).await;
    let mut call_ref = String::new();
    {
        let (_, backup) = pair(&mut w_b1, &mut w_b2, &primary_ord);
        for _ in 0..50 {
            if let Some(found) = backup.scan_one_backed_up(&primary_ord).await {
                call_ref = found;
                break;
            }
            fh.advance(Duration::from_millis(100)).await;
        }
    }
    assert!(!call_ref.is_empty(), "the backup holds the replicated call");

    {
        let (primary, backup) = pair(&mut w_b1, &mut w_b2, &primary_ord);
        primary.crash();
        proxy.set_health(&primary_ord, WorkerHealth::Dead);
        backup.simulate_peer_removed(&primary_ord);
    }
    fh.advance(Duration::from_millis(300)).await;

    // bob hangs up; the rewriter aims the BYE at a URI that is not ours.
    let mut bye = bob_dialog.bye().await;
    let rewritten = Uri::parse(&SipStr::owned(REWRITTEN)).expect("a SIP URI");
    rewriter
        .forward_request_altered(addr(LB), |req| {
            req.thaw().with_uri(rewritten).freeze().expect("a request stays complete")
        })
        .await;
    alice.receive("BYE").await.respond(200, "OK").await;
    rewriter.forward_response(addr(BOB)).await;
    bye.expect(200).await;

    // The primary returns within its budget and discharges the deferral.
    fh.advance(Duration::from_secs(60)).await;
    {
        let (primary, backup) = pair(&mut w_b1, &mut w_b2, &primary_ord);
        fh.mark(&primary_ord, None, "reboot", "restart empty, higher gen, new pod IP");
        let new_addr = primary.reboot().await;
        proxy.set_address(&primary_ord, new_addr);
        fh.note_worker_rebound(&primary_ord, new_addr);
        backup.simulate_peer_added(&primary_ord);
        for _ in 0..120 {
            fh.advance(Duration::from_millis(500)).await;
            if primary.is_ready() {
                break;
            }
        }
        assert!(primary.is_ready(), "the rebooted primary became ready");
        proxy.set_health(&primary_ord, WorkerHealth::Alive);
    }
    fh.advance(Duration::from_secs(10)).await;

    let _ = fh
        .settle_terminal(async || {
            w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    assert_eq!(total_cdrs_for(&[&w_b1, &w_b2], &call_ref), 1, "the call has its one CDR");
    let cdr = [&w_b1, &w_b2]
        .iter()
        .flat_map(|n| n.cdr_records())
        .find(|r| r.call_ref == call_ref)
        .expect("the one CDR");
    let bye = cdr
        .events
        .iter()
        .find(|e| e.event_type == call::CdrEventType::Bye)
        .unwrap_or_else(|| panic!("the BYE is recorded: {:?}", cdr.events));
    assert_eq!(cdr.b_legs.len(), 1, "{cdr:?}");
    assert_eq!(bye.leg_id, cdr.b_legs[0].leg_id, "the callee's BYE is the outgoing leg's");
    let termination = cdr.termination.as_ref().expect("the record names who ended the call");
    assert_eq!(termination.by_leg.as_deref(), Some(cdr.b_legs[0].leg_id.as_str()));
}
