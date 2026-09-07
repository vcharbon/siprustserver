//! **A responder's retransmission across a node kill** (issue 264 items B/C,
//! the takeover half): the record of what this stack has already PRACKed is
//! replicated, so a survivor absorbs a repeat exactly as the primary would.
//!
//! Where the `relayFirst18xTo180` policy masks the callee's provisional behind a
//! bare 180, the caller is shown nothing reliable and the responder's provisional
//! is THIS STACK's to acknowledge.
//! RFC 3262 §3 has the responder repeat until his PRACK arrives, so a copy
//! crossing the acknowledgement is ordinary — and §4 makes every copy after the
//! first a retransmission the receiver discards. `Call.pracked_provisionals`
//! carries that answer, and it is replicated for exactly this moment: a repeat
//! arriving on a node that never sent the PRACK must still die there. A survivor
//! that re-PRACKed would draw §3's 481 from a responder who has already
//! acknowledged — and that 481 is what `prack_481_keeps_the_call.rs` pins.
//!
//! ```text
//!   INVITE(100rel) → [b2bua strips 100rel] → 183(100rel,RSeq 4711)
//!          ← bare 180 ; PRACK → 200(PRACK)      ✗ primary killed ✗
//!                       → 183(100rel,RSeq 4711) [bob's §3 repeat, at the survivor]
//!          ← nothing   ; NO second PRACK
//!          → 200 → ACK → BYE → 200(BYE)
//! ```
//!
//! The sibling cell for `reliable_provisionals` — a PRACK the SURVIVOR must
//! translate — is `prack_takeover.rs` (issue 263).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to_with_18x;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use call::features::RelayFirst18xStrategy;
use call::{CallBodyCodec, MsgpackCodec, PrackedProvisional};
use b2bua::limiter::NoopLimiter;
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness, PartitionRole,
    ProxySut, ReplicatedB2buaSut, WorkerHealth,
};
use sip_message::header::{RSeq, Require};
use sip_message::types::SipResponse;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

/// Bob's own sequence — far from anything this stack mints first.
const BOB_RSEQ: u32 = 4711;

fn requires_100rel(resp: &SipResponse) -> bool {
    resp.header::<Require>().and_then(Result::ok).is_some_and(|r| r.contains("100rel"))
}

/// A decision that MASKS the callee's first 18x behind a bare 180 (`drop-sdp`):
/// the caller is shown no reliable provisional, so the callee's is this stack's
/// to acknowledge.
fn decision_masking_the_first_18x() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                NewCallResponse::Route(route_to_with_18x(
                    "127.0.0.1",
                    5070,
                    RelayFirst18xStrategy::DropSdp,
                ))
            })
            .build(),
    )
}

/// The replicated `PrackedProvisional` entries the backup holds for `call_ref`.
async fn replicated_pracked(
    backup: &ReplicatedB2buaSut,
    primary: &str,
    call_ref: &str,
) -> Vec<PrackedProvisional> {
    let body = backup
        .get(PartitionRole::Backup, primary, call_ref)
        .await
        .expect("the backup holds a replica body for the call");
    MsgpackCodec::new().decode(&body).expect("replica body decodes").pracked_provisionals
}

/// Reboot the crashed primary EMPTY at a higher gen + new pod IP and let the
/// go-active reclaim run, so the call's CDR authority returns to discharge the
/// terminal the survivor deferred.
async fn reboot_and_reclaim(
    fh: &mut FailoverHarness,
    primary: &mut ReplicatedB2buaSut,
    survivor: &ReplicatedB2buaSut,
    primary_ord: &str,
    proxy: &ProxySut,
) {
    fh.mark(primary_ord, None, "reboot", "restart empty, higher gen, new pod IP");
    let new_addr = primary.reboot().await;
    proxy.set_address(primary_ord, new_addr);
    fh.note_worker_rebound(primary_ord, new_addr);
    survivor.simulate_peer_added(primary_ord);
    for _ in 0..120 {
        fh.advance(Duration::from_millis(500)).await;
        if primary.is_ready() {
            break;
        }
    }
    assert!(primary.is_ready(), "rebooted primary {primary_ord} became ready");
    proxy.set_health(primary_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_secs(10)).await;
}

#[tokio::test(start_paused = true)]
async fn a_repeat_after_takeover_draws_no_second_prack_from_the_survivor() {
    let mut fh = FailoverHarness::new("prack-repeat-takeover", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let decision = decision_masking_the_first_18x();
    let mut w_b1 = fh
        .spawn_worker_limited(
            "b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080),
            decision.clone(), Arc::new(NoopLimiter),
        )
        .await;
    let mut w_b2 = fh
        .spawn_worker_limited(
            "b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080),
            decision.clone(), Arc::new(NoopLimiter),
        )
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");

    // ── STEP 1: alice offers 100rel; the mask strips it from bob's INVITE ────
    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(proxy.addr())
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _) = worker_ordinals(uas.request());
    let (primary, survivor): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };

    // Bob answers reliably, in good faith; alice is shown the ordinary copy.
    uas.respond(183, "Session Progress")
        .reliable(BOB_RSEQ)
        .with_sdp(ANSWER)
        .await;
    let p180 = call.expect(180).await;
    assert!(!requires_100rel(&p180), "the mask shows the caller nothing reliable");
    assert!(p180.header::<RSeq>().is_none(), "and no RSeq");
    assert!(p180.body().is_empty(), "the bare 180 carries no body");

    // This stack offered the extension to bob, so it acknowledges him itself.
    bob.receive("PRACK").await.respond(200, "OK").await;

    // ── STEP 2: the answer is REPLICATED before the kill ─────────────────────
    // The cell must test the reachable case: an entry that never replicated
    // leaves the survivor nothing to absorb with, and that case has no fix.
    fh.advance(Duration::from_millis(500)).await;
    let call_ref = survivor
        .scan_one_backed_up(&pri_ord)
        .await
        .expect("the early call replicated to the backup");
    let entries = replicated_pracked(survivor, &pri_ord, &call_ref).await;
    assert_eq!(
        entries.len(),
        1,
        "the backup must hold the record of what this stack PRACKed at the moment of \
         the kill; got {entries:?}",
    );
    assert_eq!(
        entries[0].rseq, BOB_RSEQ as i64,
        "the replicated entry carries the number bob stated",
    );

    // ── STEP 3: kill the primary ─────────────────────────────────────────────
    fh.mark(&pri_ord, None, "crash", "primary down after PRACKing the callee");
    primary.crash();
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    survivor.simulate_peer_removed(&pri_ord);
    fh.advance(Duration::from_millis(300)).await;
    let hydrated_before = survivor.metrics().repl_takeover_hydrated_total();

    // ── STEP 4: bob's §3 ladder repeats the SAME provisional, at the survivor ─
    uas.respond(183, "Session Progress")
        .reliable(BOB_RSEQ)
        .with_sdp(ANSWER)
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(
        bob.try_receive_tolerating("PRACK", &[]).await.is_none(),
        "the survivor absorbs a repeat of a provisional the cluster has already \
         acknowledged (RFC 3262 §4) — hydrated books absorb as well as they translate",
    );
    // The silence above is only evidence if the repeat REACHED the survivor and
    // was read as this call's: a datagram dropped before hydration would draw no
    // PRACK either, and prove nothing.
    let survivor_addr: SocketAddr =
        if pri_ord == "b1" { B2 } else { B1 }.parse().expect("survivor sip addr");
    let at_survivor = fh
        .sip_entries()
        .iter()
        .filter(|e| e.to == survivor_addr && e.raw.starts_with(b"SIP/2.0 183 "))
        .count();
    assert_eq!(at_survivor, 1, "bob's repeat reached the survivor's bind");
    assert!(
        survivor.metrics().repl_takeover_hydrated_total() > hydrated_before,
        "and hydrated the call onto it — the repeat was read as this call's, then absorbed",
    );

    // ── STEP 5: the call answers on the survivor and tears down cleanly ──────
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(300)).await;

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;

    // ── STEP 6: the primary returns and discharges ───────────────────────────
    fh.advance(Duration::from_secs(60)).await;
    {
        let (primary, survivor): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
            if pri_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
        reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    }
    let _ = fh
        .settle_terminal(async || {
            w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;

    assert_eq!(
        total_cdrs_for(&[&w_b1, &w_b2], &call_ref),
        1,
        "exactly one CDR for the failed-over call across the cluster",
    );
    assert_call_fully_released(&[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}
