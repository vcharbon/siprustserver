//! **A turn past the record-size cap fails on a backed call as on a lone
//! one** (ADR-0057 §2, §4). alice's INFO carries a body the record keeps
//! while the request is in flight, so the turn on it lands a record past the
//! cap: the INFO is answered 500 and the verdict tears the call down on the
//! wire. On a takeover copy the cap reads the record the copy built from its
//! primary's replica, and the CDR is written once, by the primary's reclaim.
//!
//! ```text
//!   alice ═ b1 ═ bob               (answered, replicated to b2)
//!   alice ─INFO (20 KiB)─▶ proxy ─▶ b1: record past 24 KiB → 500
//!   b1 ─BYE─▶ alice, b1 ─BYE─▶ bob: one CDR
//!
//!   ✗ b1 killed, unannounced ✗
//!   alice ─INFO (20 KiB)─▶ proxy ─▶ b2 (copy): record past 24 KiB → 500
//!   b2 ─BYE─▶ alice, b2 ─BYE─▶ bob; b1 reboots and reclaims: one CDR
//! ```

use b2bua::config::Cap;
use failover_harness::FailoverHarness;
use scenario_harness::{Agent, Dialog};
use sip_message::generators::InDialogMethod;

use crate::departure_support::{bring_up_reactive, placed};
use crate::inflight_resend::{
    kill_serving_node, reboot_and_reclaim, settle_to_one_cdr, split_mut, survivor_of, REPLICATE,
};

/// The record-size cap of both workers, bytes.
const RECORD_BYTES: u64 = 24 << 10;

/// The harness of `name`, its workers under [`RECORD_BYTES`].
fn capped(name: &str) -> FailoverHarness {
    FailoverHarness::new(name, &["b1", "b2"]).with_worker_tune(|c| {
        c.caps.record_bytes = RECORD_BYTES;
    })
}

/// alice's INFO on `dialog`, whose body takes the record past
/// [`RECORD_BYTES`], is answered 500 and both legs are BYEd.
async fn info_past_the_cap_ends_the_call(dialog: &mut Dialog, alice: &Agent, bob: &Agent) {
    let body = vec![b'x'; 20 << 10];
    let mut info = dialog
        .send_request(InDialogMethod::Info)
        .with_body("application/example-binary", body)
        .send()
        .await;
    info.expect(500).await;
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
}

#[tokio::test(start_paused = true)]
async fn a_backed_primary_past_the_record_size_cap_answers_500_and_byes_both_legs() {
    let name = "record-cap-backed-primary";
    let mut fh = capped(name);
    let alice = fh.agent("alice", "127.0.0.1:5060").await;
    let bob = fh.agent("bob", "127.0.0.1:5070").await;
    let (proxy, w_b1, w_b2) = bring_up_reactive(&mut fh).await;

    let mut dialog = scenario_harness::callflow::establish(&alice, &bob, proxy.addr()).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    fh.advance(REPLICATE).await;

    info_past_the_cap_ends_the_call(&mut dialog, &alice, &bob).await;
    let primary = if pri_ord == "b1" { &w_b1 } else { &w_b2 };
    assert_eq!(primary.metrics().cap_trips_of(Cap::RecordBytes), 1, "the primary's turn tripped");
    assert_eq!(primary.metrics().reaper_discharged_total(), 0, "the verdict's teardown ran");
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}

#[tokio::test(start_paused = true)]
async fn a_takeover_copy_past_the_record_size_cap_answers_500_and_byes_both_legs() {
    let name = "record-cap-on-a-takeover-copy";
    let mut fh = capped(name);
    let alice = fh.agent("alice", "127.0.0.1:5060").await;
    let bob = fh.agent("bob", "127.0.0.1:5070").await;
    let (proxy, mut w_b1, mut w_b2) = bring_up_reactive(&mut fh).await;

    let mut dialog = scenario_harness::callflow::establish(&alice, &bob, proxy.addr()).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    fh.advance(REPLICATE).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;

    info_past_the_cap_ends_the_call(&mut dialog, &alice, &bob).await;
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated + 1,
        "the INFO took the call over on the backup"
    );
    assert_eq!(survivor.metrics().cap_trips_of(Cap::RecordBytes), 1, "the copy's turn tripped");
    assert_eq!(survivor.metrics().reaper_discharged_total(), 0, "the verdict's teardown ran");

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
