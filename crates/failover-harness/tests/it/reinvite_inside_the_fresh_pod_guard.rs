//! **A re-INVITE inside a returned primary's fresh-pod guard.** The primary of
//! an established call crashes, its replacement comes up at a new address,
//! reclaims the call and is read alive by the front proxy. The proxy keeps
//! in-dialog requests off a freshly joined endpoint for its guard window and
//! sends them to the cookie's backup, while an ACK or CANCEL follows an alive
//! primary. The caller re-INVITEs inside that window and the callee answers:
//! the backup's takeover copy relays the round and its 2xx, the caller's ACK
//! reaches the primary.
//!
//! ```text
//!   established on P · backup S holds the replica
//!   crash P          P dies; its endpoint leaves the slice
//!   +1 s             P comes back at a new address, joins, reclaims the call
//!   ready            the proxy reads P alive; the fresh-pod guard runs
//!   +2 s             caller re-INVITE → proxy → S (guard) → callee
//!                    callee 200 → S → caller; caller ACK → proxy → P
//! ```
//!
//! RFC 3261 §13.3.1.4 ends the 2xx retransmissions on the ACK, whichever
//! node sent the 2xx; the dialog then lives on (no BYE from the B2BUA after
//! 64·T1) until the caller's BYE, and the call settles to one CDR.

use std::time::Duration;

use failover_harness::{assert_call_fully_over, FailoverHarness, WorkerHealth};
use scenario_harness::Inbound;
use sip_message::generators::InDialogMethod;
use sip_message::Method;

use crate::call_terminate_on_backup::{establish, Established, ANSWER, OFFER};
use crate::withdrawn_primary_answers_in_its_window::report_dir;

/// How long the crashed primary stays away before its replacement joins.
const AWAY: Duration = Duration::from_secs(1);
/// How long after the primary reads alive the caller re-INVITEs: well inside
/// the proxy's fresh-pod guard.
const REINVITE_AFTER_ALIVE: Duration = Duration::from_secs(2);
/// How long a copy of the 2xx may still reach the caller after her ACK: past
/// the third rung of the §13.3.1.4 ladder (T1, 2·T1, 4·T1), which covers an
/// ACK the primary drops before it holds the round.
const ACK_WINDOW: Duration = Duration::from_secs(4);
/// From the end of [`ACK_WINDOW`] to past the §13.3.1.4 give-up (64·T1 = 32 s).
const PAST_GIVE_UP: Duration = Duration::from_secs(36);
/// The clock step the caller and the callee read their sockets at.
const STEP: Duration = Duration::from_millis(100);

/// What reached the parties after the caller's ACK.
#[derive(Default)]
struct AfterAck {
    /// Copies of the re-INVITE's 2xx the caller took (each re-ACKed).
    answer_copies: usize,
    /// ACKs the callee drew for its 2xx.
    acks_to_callee: usize,
    /// BYEs the B2BUA sent the caller.
    byes_to_caller: usize,
    /// BYEs the B2BUA sent the callee.
    byes_to_callee: usize,
}

/// Pump `window`, answering both parties as conformant UAs: the caller
/// re-ACKs every copy of the 2xx (RFC 3261 §13.2.2.4), either party answers a
/// BYE or a keepalive OPTIONS 200.
async fn observe(
    fh: &FailoverHarness,
    alice: &scenario_harness::Agent,
    bob: &scenario_harness::Agent,
    dialog: &mut scenario_harness::Dialog,
    window: Duration,
    out: &mut AfterAck,
) {
    let mut waited = Duration::ZERO;
    while waited < window {
        fh.advance(STEP).await;
        waited += STEP;
        while let Some(msg) = alice.take_queued_inbound().await {
            match msg {
                Inbound::Response(r)
                    if r.status() == 200 && *r.cseq().method() == Method::Invite =>
                {
                    out.answer_copies += 1;
                    dialog.ack(None).await;
                }
                Inbound::Response(_) => {}
                Inbound::Request(mut t) => {
                    if t.request().method() == "BYE" {
                        out.byes_to_caller += 1;
                    }
                    t.respond(200, "OK").await;
                }
            }
        }
        while let Some(msg) = bob.take_queued_inbound().await {
            match msg {
                Inbound::Request(t) if t.request().method() == "ACK" => out.acks_to_callee += 1,
                Inbound::Request(mut t) => {
                    if t.request().method() == "BYE" {
                        out.byes_to_callee += 1;
                    }
                    t.respond(200, "OK").await;
                }
                Inbound::Response(_) => {}
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_reinvite_inside_the_fresh_pod_guard_is_acked_and_the_call_lives_on() {
    let name = "reinvite-inside-the-fresh-pod-guard";
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
    fh.readmit_fresh(&pri, new_addr);
    let reclaimed = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
            primary.is_ready() && primary.serves(&call_ref)
        })
        .await;
    assert!(reclaimed, "the replacement became ready and reclaimed the established call");
    proxy.set_health(&pri, WorkerHealth::Alive);
    fh.mark(&pri, None, "alive", "the proxy reads the returned primary alive");
    fh.advance(REINVITE_AFTER_ALIVE).await;

    // ── the caller re-INVITEs inside the guard; the callee answers ───────────
    fh.mark(&pri, None, "re-INVITE", "the caller re-INVITEs inside the fresh-pod guard");
    let mut round = dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    let mut at_bob = bob.receive_tolerating("INVITE", &["OPTIONS"]).await;
    fh.advance(Duration::from_millis(200)).await;
    let to_survivor = fh.sip_entries().iter().any(|e| {
        e.from == proxy.addr() && e.to == survivor.sip_addr() && e.raw.starts_with(b"INVITE ")
    });
    assert!(to_survivor, "the fresh-pod guard sent the re-INVITE to the cookie's backup");
    fh.mark(
        &pri,
        None,
        "round served",
        &format!(
            "primary({}) serves={} survivor({}) serves={}",
            primary.sip_addr(),
            primary.serves(&call_ref),
            survivor.sip_addr(),
            survivor.serves(&call_ref),
        ),
    );
    at_bob.respond(200, "OK").with_sdp(ANSWER).await;
    round.expect_tolerating(200, &["OPTIONS"]).await;
    dialog.ack(None).await;

    // ── the ACK window, then past the §13.3.1.4 give-up ──────────────────────
    let mut early = AfterAck::default();
    observe(&fh, &alice, &bob, &mut dialog, ACK_WINDOW, &mut early).await;
    let mut late = AfterAck::default();
    observe(&fh, &alice, &bob, &mut dialog, PAST_GIVE_UP, &mut late).await;
    let acks_to_callee = early.acks_to_callee + late.acks_to_callee;
    let byes =
        (early.byes_to_caller + late.byes_to_caller, early.byes_to_callee + late.byes_to_callee);
    fh.mark(
        &pri,
        None,
        "after give-up window",
        &format!(
            "2xx copies to caller: {} in the ACK window, {} after it; ACKs to callee={}; \
             BYEs to caller={} to callee={}",
            early.answer_copies, late.answer_copies, acks_to_callee, byes.0, byes.1,
        ),
    );
    let lives_on = byes == (0, 0);

    // ── the caller hangs up an established call ──────────────────────────────
    if lives_on {
        let mut bye = dialog.bye().await;
        bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
        bye.expect_tolerating(200, &["OPTIONS"]).await;
    }
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

    let passed = lives_on && acks_to_callee == 1 && late.answer_copies == 0 && drained;
    let written = fh
        .write_unified_report(
            &report_dir(name),
            "report",
            "A re-INVITE inside a returned primary's fresh-pod guard",
            passed,
        )
        .expect("report written");
    eprintln!("report: {}", written[0].display());

    assert_eq!(
        late.answer_copies, 0,
        "the caller's ACK ends the 2xx retransmissions (RFC 3261 §13.3.1.4): {} copies \
         reached her past the ACK window",
        late.answer_copies,
    );
    assert_eq!(byes, (0, 0), "the B2BUA BYEs neither party (caller, callee)");
    assert_eq!(acks_to_callee, 1, "the callee draws exactly one ACK for its 2xx");
    assert!(drained, "the call drains cleanly");
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &store).await;
    fh.assert_sip_rfc_clean(name);
}
