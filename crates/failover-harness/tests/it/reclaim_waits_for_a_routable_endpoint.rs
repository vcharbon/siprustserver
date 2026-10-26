//! **A returning node re-serves its calls only once its endpoint is routable.**
//! A node that comes back after a crash and a departure takes back its own
//! calls only once its own endpoint reads ready in the slice again. Before
//! that, the proxy cannot route a response to it: a response to a request the
//! node sent goes to the call's other owner (ADR-0055 §8), and that owner
//! reads a final above its local CSeq as evidence of a lost write and ends the
//! call (§4, trigger 1).
//!
//! ```text
//!   P crashes; S keeps the quiet call's copy
//!   P away past the call's keepalive deadline, reboots at a new address
//!   departed   P left the slice; ready, it is not yet published
//!   flapped    P's first snapshot still lists it ready, then its endpoint
//!              reads not ready (the restarted container), and P becomes ready
//!   nothing reaches either party
//!   P reads ready in the slice: its keepalive OPTIONS reaches both parties,
//!   their 200s reach P
//!   the call lives on to the caller's BYE
//! ```
//!
//! Every cell ends with the caller's BYE, one CDR and clean memory.

use std::time::Duration;

use failover_harness::{assert_call_fully_over, WorkerHealth};
use scenario_harness::Agent;

use crate::call_terminate_on_backup::{establish, Established};

/// How long the crashed node stays away: past the established call's 300 s
/// keepalive deadline, so the reclaimed keepalive is due the moment the call
/// is reclaimed.
const AWAY: Duration = Duration::from_secs(310);

/// How long the returned node stays ready and unpublished.
const UNPUBLISHED: Duration = Duration::from_secs(5);

/// Every in-dialog OPTIONS the proxy sent to `to`.
fn options_sent_to(fh: &failover_harness::FailoverHarness, to: &Agent) -> usize {
    fh.sip_entries().iter().filter(|e| e.to == to.addr() && e.raw.starts_with(b"OPTIONS ")).count()
}

/// Answer the keepalive OPTIONS `agent` holds, if any.
async fn answer_keepalive(agent: &Agent) -> bool {
    match agent.try_receive_tolerating("OPTIONS", &[]).await {
        Some(mut probe) => {
            probe.respond(200, "OK").await;
            true
        }
        None => false,
    }
}

/// How the returning node's endpoint leaves the slice and comes back.
#[derive(Clone, Copy)]
enum Shape {
    /// The endpoint left the slice; the returned node is published again.
    Departed,
    /// The endpoint stays in the slice: listed ready when the node boots, not
    /// ready from then on, ready again once the node is.
    Flapped,
}

/// P crashes, departs, and returns past its call's keepalive deadline. Ready
/// but unpublished, it sends nothing; once published, its keepalive is
/// answered at P and the call stays up until the caller hangs up.
#[tokio::test(start_paused = true)]
async fn a_returned_node_probes_its_reclaimed_call_only_once_routable() {
    returns_and_probes("reclaim-waits-for-a-routable-endpoint", Shape::Departed).await;
}

/// P crashes and restarts with its endpoint in the slice: its first snapshot
/// still lists it ready, then it reads not ready. Ready itself, it sends
/// nothing until its endpoint reads ready again.
#[tokio::test(start_paused = true)]
async fn a_restarted_node_whose_endpoint_flaps_probes_only_once_ready_again() {
    returns_and_probes("reclaim-waits-for-a-ready-endpoint", Shape::Flapped).await;
}

async fn returns_and_probes(name: &str, shape: Shape) {
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

    // ── the primary crashes ──────────────────────────────────────────────────
    fh.mark(&pri, None, "crash", "the primary dies");
    primary.crash();
    proxy.set_health(&pri, WorkerHealth::Dead);
    if let Shape::Departed = shape {
        fh.depart(&pri);
    }
    fh.advance(AWAY).await;

    // ── it reboots at a new address and becomes ready, its endpoint not ──────
    fh.mark(&pri, None, "reboot", "restart empty, higher gen, new address");
    let new_addr = primary.reboot().await;
    fh.note_worker_rebound(&pri, new_addr);
    if let Shape::Flapped = shape {
        fh.flap_not_ready(&pri);
    }
    let ready = fh
        .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
            primary.is_ready()
        })
        .await;
    assert!(ready, "the returned node became ready");
    fh.advance(UNPUBLISHED).await;
    assert_eq!(options_sent_to(&fh, &alice), 0, "no OPTIONS reached the caller before publication");
    assert_eq!(options_sent_to(&fh, &bob), 0, "no OPTIONS reached the callee before publication");
    assert!(!survivor.serves(&call_ref), "the call's other owner took nothing over");

    // ── published: the reclaimed keepalive probes both parties ───────────────
    match shape {
        Shape::Departed => fh.readmit(&pri, new_addr),
        Shape::Flapped => {
            fh.flap_ready(&pri);
            proxy.set_address(&pri, new_addr);
        }
    }
    let mut answered = (false, false);
    for _ in 0..50 {
        fh.advance(Duration::from_millis(100)).await;
        answered.0 |= answer_keepalive(&alice).await;
        answered.1 |= answer_keepalive(&bob).await;
        if answered == (true, true) {
            break;
        }
    }
    assert_eq!(answered, (true, true), "the published node probed both parties");
    fh.advance(Duration::from_secs(2)).await;
    assert!(primary.serves(&call_ref), "the returned node serves the call it reclaimed");
    proxy.set_health(&pri, WorkerHealth::Alive);
    assert!(!survivor.serves(&call_ref), "the call's other owner took nothing over");
    assert!(alice.take_queued().await.is_none(), "the caller holds no BYE");
    assert!(bob.take_queued().await.is_none(), "the callee holds no BYE");

    // ── the caller hangs up through the returned node ────────────────────────
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
