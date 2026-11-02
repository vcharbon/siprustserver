//! **A liveness probe lost with its primary never ends the call.** The primary
//! sends its in-dialog OPTIONS to both parties (RFC 3261 §11) and its record
//! reaches the backup; the primary dies before either 200 is read. A copy
//! restored past the probe's Timer F (RFC 3261 §17.1.2.2) — the returned
//! primary's reclaim, or the backup's takeover on the caller's next request —
//! reads nothing of that probe: both parties are up, the call stands and the
//! next interval probes again.
//!
//! ```text
//!   b1 ─OPTIONS─▶ alice, bob   record applied at b2
//!   ✗ b1 dies ✗   alice, bob ─200─▶ (lost)
//!   > Timer F later:
//!   reclaimed   b1 returns, reclaims · the next probe answered · alice BYE
//!   taken over  alice ─INFO─▶ b2 ─INFO─▶ bob, 200 · alice BYE
//! ```
//!
//! Beyond the failover sweep: a restore discards the probe in flight at the crash.

use std::time::Duration;

use b2bua::store::envelope;
use call::{CallModelState, MsgpackCodec};
use failover_harness::{
    assert_call_fully_over, FailoverHarness, PartitionRole, ReplicatedB2buaSut, WorkerHealth,
};
use scenario_harness::{Agent, Dialog};
use sip_message::generators::InDialogMethod;

use crate::call_terminate_on_backup::{establish, Established};

/// Past the probe's Timer F (64·T1, RFC 3261 §17.1.2.2) by a margin.
const PAST_TIMER_F: Duration = Duration::from_secs(40);

/// Pump the clock until both parties hold the keepalive probe, unanswered, in
/// steps short of its first retransmission; bounded by one interval.
async fn probe_reaches_both(
    fh: &FailoverHarness,
    alice: &Agent,
    bob: &Agent,
) -> (scenario_harness::agent::ServerTxn, scenario_harness::agent::ServerTxn) {
    let mut at_alice = None;
    let mut at_bob = None;
    let reached = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(340), async || {
            if at_alice.is_none() {
                at_alice = alice.try_receive_tolerating("OPTIONS", &[]).await;
            }
            if at_bob.is_none() {
                at_bob = bob.try_receive_tolerating("OPTIONS", &[]).await;
            }
            at_alice.is_some() && at_bob.is_some()
        })
        .await;
    assert!(reached, "the keepalive probe reached both parties within an interval");
    (at_alice.expect("alice's probe"), at_bob.expect("bob's probe"))
}

/// Whether `backup`'s replica of `call_ref` holds a keepalive probe's record.
async fn replica_holds_a_probe(backup: &ReplicatedB2buaSut, primary: &str, call_ref: &str) -> bool {
    let Some(body) = backup.get(PartitionRole::Backup, primary, call_ref).await else {
        return false;
    };
    let call = envelope::decode(&MsgpackCodec::new(), &body).expect("the replica decodes").call;
    let holds = call::helpers::every_record(&call)
        .any(|(_, _, r)| r.author.as_deref() == Some(call::KEEPALIVE_AUTHOR));
    holds
}

/// The call established on its primary, the primary's probe at both parties
/// and its record applied at the backup; then the primary dies, leaves the
/// slice. Both parties answer the probe while the proxy still reads the
/// primary alive, so both 200s are lost. Returns the established call with
/// the primary down, past the probe's Timer F.
async fn probe_lost_with_the_primary(name: &str) -> Established {
    let mut est = establish(name).await;
    let (at_alice, at_bob) = probe_reaches_both(&est.fh, &est.alice, &est.bob).await;
    let pri = est.primary_ord.clone();
    let backup = if pri == "b1" { &est.w_b2 } else { &est.w_b1 };
    let applied = est
        .fh
        .pump_until(Duration::from_millis(50), Duration::from_millis(400), async || {
            replica_holds_a_probe(backup, &pri, &est.call_ref).await
        })
        .await;
    assert!(applied, "the probe's record reached the backup before the crash");

    let primary = if pri == "b1" { &mut est.w_b1 } else { &mut est.w_b2 };
    est.fh.mark(&pri, None, "crash", "the primary dies with its probe in flight");
    primary.crash();
    // The proxy still reads the primary alive: both 200s go to its dead
    // address and are lost.
    for mut probe in [at_alice, at_bob] {
        probe.respond(200, "OK").await;
    }
    est.fh.advance(Duration::from_millis(100)).await;
    est.proxy.set_health(&pri, WorkerHealth::Dead);
    est.fh.depart(&pri);
    est.fh.advance(PAST_TIMER_F).await;
    est
}

/// The caller hangs up; the call drains to one CDR and clean memory.
async fn caller_hangs_up(
    name: &str,
    fh: &mut FailoverHarness,
    (alice, bob, dialog): (&Agent, &Agent, &mut Dialog),
    (w_b1, w_b2): (&ReplicatedB2buaSut, &ReplicatedB2buaSut),
    call_ref: &str,
    store: &std::sync::Arc<call_limiter::CallStore>,
) {
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    let drained = fh
        .settle_terminal(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(call_ref).await
                && !w_b2.holds_any_trace(call_ref).await
        })
        .await;
    fh.linger_peers(&[alice, bob], Duration::from_secs(3)).await;
    assert!(drained, "the call drains cleanly");
    assert_call_fully_over(&[w_b1, w_b2], call_ref, store).await;
    fh.assert_sip_rfc_clean(name);
}

/// The primary returns past the probe's Timer F and reclaims the call: the
/// call stands, the next interval probes both parties, they answer, and the
/// call lives on to the caller's BYE.
#[tokio::test(start_paused = true)]
async fn the_reclaimed_call_stands_and_probes_again() {
    let name = "probe-lost-with-the-primary-reclaimed";
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
    } = probe_lost_with_the_primary(name).await;

    let primary = if pri == "b1" { &mut w_b1 } else { &mut w_b2 };
    fh.mark(&pri, None, "reboot", "restart empty, higher gen, new address");
    let new_addr = primary.reboot().await;
    fh.note_worker_rebound(&pri, new_addr);
    fh.readmit(&pri, new_addr);
    let reclaimed = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
            primary.is_ready() && primary.serves(&call_ref)
        })
        .await;
    assert!(reclaimed, "the returned primary reclaimed the call");
    proxy.set_health(&pri, WorkerHealth::Alive);
    fh.advance(Duration::from_secs(1)).await;
    assert_eq!(
        primary.live_call(&call_ref).map(|c| c.state),
        Some(CallModelState::Active),
        "the reclaimed call stands: the probe lost with the crash is not read as unanswered"
    );

    let (mut at_alice, mut at_bob) = probe_reaches_both(&fh, &alice, &bob).await;
    at_alice.respond(200, "OK").await;
    at_bob.respond(200, "OK").await;
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(
        primary.live_call(&call_ref).map(|c| c.state),
        Some(CallModelState::Active),
        "the answered probe keeps the call up"
    );

    caller_hangs_up(name, &mut fh, (&alice, &bob, &mut dialog), (&w_b1, &w_b2), &call_ref, &store)
        .await;
}

/// Past the probe's Timer F the caller's INFO reaches the backup, which takes
/// the call over: the INFO is relayed and answered, the call stands, and it
/// lives on to the caller's BYE.
#[tokio::test(start_paused = true)]
async fn the_taken_over_call_stands() {
    let name = "probe-lost-with-the-primary-taken-over";
    let Established {
        mut fh,
        alice,
        bob,
        proxy: _proxy,
        store,
        lh: _lh,
        w_b1,
        w_b2,
        primary_ord: pri,
        call_ref,
        mut dialog,
        ..
    } = probe_lost_with_the_primary(name).await;
    let backup = if pri == "b1" { &w_b2 } else { &w_b1 };

    let mut info = dialog.request(InDialogMethod::Info, None).await;
    bob.receive("INFO").await.respond(200, "OK").await;
    info.expect(200).await;
    fh.advance(Duration::from_secs(1)).await;
    assert_eq!(
        backup.live_call(&call_ref).map(|c| c.state),
        Some(CallModelState::Active),
        "the taken-over call stands: the probe lost with the crash is not read as unanswered"
    );

    caller_hangs_up(name, &mut fh, (&alice, &bob, &mut dialog), (&w_b1, &w_b2), &call_ref, &store)
        .await;
}
