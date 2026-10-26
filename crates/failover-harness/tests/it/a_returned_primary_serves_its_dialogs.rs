//! **A worker that returns serves its own dialogs at once.** The proxy routes
//! an in-dialog request to the cookie's primary while the primary is alive,
//! however recently it joined (ADR-0050 §8): its Ready gate already waited
//! for its partition to be bootstrapped and current, and the calls it held
//! before it left are taken back by on-demand reclaim.
//!
//! ```text
//!   returned   established on P · P crashes, comes back at a new address,
//!              reclaims the call, is read alive
//!              the caller's re-INVITE right after → P, answered, ACKed, BYE
//!   joined     P crashes and comes back; S refuses new calls, so the next
//!              call is placed on P just after it joined
//!              its re-INVITE and BYE → P, no takeover
//! ```
//!
//! Every cell ends with the caller's BYE, one CDR and clean memory.

use std::time::Duration;

use failover_harness::{
    assert_call_fully_over, assert_call_fully_released, total_cdrs_for, worker_ordinals,
    FailoverHarness, WorkerHealth,
};
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};

use crate::call_terminate_on_backup::{establish, Established, ANSWER, OFFER};
use crate::inflight_resend::{REANSWER, REOFFER};
use crate::new_call_acceptance::{bring_up, PROBE_ROUND};

/// How long the crashed primary stays away before its replacement joins.
const AWAY: Duration = Duration::from_secs(1);

/// How many INVITEs carrying `offer` the proxy forwarded to `to`.
fn invites_forwarded_to(
    fh: &FailoverHarness,
    proxy: std::net::SocketAddr,
    to: std::net::SocketAddr,
    offer: &str,
) -> usize {
    let parser = CustomParser::new();
    fh.sip_entries()
        .iter()
        .filter(|e| e.from == proxy && e.to == to)
        .filter(|e| {
            matches!(parser.parse(&e.raw), Ok(SipMessage::Request(r))
                if *r.method() == Method::Invite && r.body() == offer.as_bytes())
        })
        .count()
}

/// P crashes, returns at a new address, reclaims the established call and is
/// read alive. The caller's re-INVITE sent right after is served by P: P
/// relays it, the callee answers, the caller's ACK closes the round, and the
/// call lives on to the caller's BYE.
#[tokio::test(start_paused = true)]
async fn a_reinvite_right_after_the_primary_returns_is_served_by_the_primary() {
    let name = "reinvite-right-after-the-primary-returns";
    let Established {
        mut fh,
        alice,
        bob,
        proxy,
        store,
        lh: _lh,
        mut w_b1,
        mut w_b2,
        primary_ord: pri,
        call_ref,
        mut dialog,
        ..
    } = establish(name).await;
    let (primary, survivor) = if pri == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };

    // ── the primary crashes; its endpoint leaves the slice ───────────────────
    fh.mark(&pri, None, "crash", "the primary dies with its endpoint");
    primary.crash();
    proxy.set_health(&pri, WorkerHealth::Dead);
    fh.depart(&pri);
    fh.advance(AWAY).await;

    // ── its replacement joins at a new address and reclaims the call ────────
    fh.mark(&pri, None, "reboot", "restart empty, higher gen, new address");
    let new_addr = primary.reboot().await;
    fh.note_worker_rebound(&pri, new_addr);
    fh.readmit(&pri, new_addr);
    let reclaimed = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
            primary.is_ready() && primary.serves(&call_ref)
        })
        .await;
    assert!(reclaimed, "the replacement became ready and reclaimed the established call");
    proxy.set_health(&pri, WorkerHealth::Alive);
    fh.mark(&pri, None, "alive", "the proxy reads the returned primary alive");
    let hydrated = survivor.metrics().repl_takeover_hydrated_total();

    // ── the caller re-INVITEs right away; the primary serves the round ───────
    let mut round = dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let mut at_bob = bob.receive_tolerating("INVITE", &["OPTIONS"]).await;
    assert_eq!(
        invites_forwarded_to(&fh, proxy.addr(), new_addr, REOFFER),
        1,
        "the proxy sent the re-INVITE to the returned primary"
    );
    assert_eq!(
        invites_forwarded_to(&fh, proxy.addr(), survivor.sip_addr(), REOFFER),
        0,
        "nothing reached the backup"
    );
    at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    round.expect_tolerating(200, &["OPTIONS"]).await;
    dialog.ack(None).await;
    bob.receive_tolerating("ACK", &["OPTIONS"]).await;
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated,
        "the backup took nothing over"
    );

    // ── the caller hangs up ──────────────────────────────────────────────────
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    let drained = fh
        .settle_terminal(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert!(drained, "the call drains cleanly");
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &store).await;
    fh.assert_sip_rfc_clean(name);
}

/// P crashes and comes back while S refuses new calls, so the next call is
/// placed on P just after it joined. Its re-INVITE and its BYE are served by
/// P, and S, its backup, never takes it over.
#[tokio::test(start_paused = true)]
async fn a_call_placed_on_a_worker_just_after_it_joined_is_served_by_it() {
    let name = "call-on-a-worker-just-after-it-joined";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", "127.0.0.1:5060").await;
    let bob = fh.agent("bob", "127.0.0.1:5070").await;
    let mut c = bring_up(&mut fh).await;

    // ── b1 crashes and comes back at a new address ───────────────────────────
    fh.mark("b1", None, "crash", "b1 dies with its endpoint");
    c.b1.crash();
    c.proxy.set_health("b1", WorkerHealth::Dead);
    fh.depart("b1");
    fh.advance(AWAY).await;
    c.breaker_b2.send_replace(true);
    let new_addr = c.b1.reboot().await;
    fh.note_worker_rebound("b1", new_addr);
    fh.readmit("b1", new_addr);
    let alive = fh
        .pump_until(Duration::from_millis(500), PROBE_ROUND, async || {
            c.b1.is_ready() && c.proxy.health("b1") == Some(WorkerHealth::Alive)
        })
        .await;
    assert!(alive, "b1 is ready and read alive by the probe");
    let takeovers = c.takeovers();

    // ── a call placed on b1 just after it joined ─────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(c.proxy.addr()).send().await;
    let mut uas = bob.receive_tolerating("INVITE", &["OPTIONS"]).await;
    assert_eq!(worker_ordinals(uas.request()), ("b1".into(), "b2".into()), "placed on b1");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive_tolerating("ACK", &["OPTIONS"]).await;
    fh.advance(Duration::from_millis(500)).await;
    let call_ref = c.b2.scan_one_backed_up("b1").await.expect("the call replicated to b2");

    // ── its re-INVITE and its BYE, on b1 ─────────────────────────────────────
    let mut round = dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let mut at_bob = bob.receive_tolerating("INVITE", &["OPTIONS"]).await;
    at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    round.expect(200).await;
    dialog.ack(None).await;
    bob.receive_tolerating("ACK", &["OPTIONS"]).await;
    assert!(c.b1.serves(&call_ref) && !c.b2.serves(&call_ref), "b1 alone serves the call");
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect(200).await;

    c.breaker_b2.send_replace(false);
    let drained = fh.settle_terminal(async || c.b1.memory_clean() && c.b2.memory_clean()).await;
    assert!(drained, "the call drains cleanly");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_eq!(c.takeovers(), takeovers, "no call was taken over");
    assert_eq!(total_cdrs_for(&c.nodes(), &call_ref), 1, "one CDR for the call");
    assert_call_fully_released(&c.nodes(), &call_ref).await;
    fh.assert_sip_rfc_clean(name);
}
