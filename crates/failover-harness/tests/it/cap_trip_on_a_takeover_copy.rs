//! **A turn past a per-call cap on a takeover copy fails as on its primary**
//! (ADR-0057 §2, §4). The caps read the record's collection lengths, which
//! the copy holds as its primary wrote them: the copy's turn on alice's INFO
//! trips the requests' cap as the primary's would, the INFO is answered 500,
//! and the copy's verdict tears the call down on the wire. The CDR is written
//! once, by the primary's reclaim.
//!
//! ```text
//!   alice ═ b1 ═ bob               (answered, replicated to b2)
//!   ✗ b1 killed, unannounced ✗
//!   alice ─INFO─▶ proxy ─▶ b2 (copy): turn past the cap → 500
//!   b2 ─BYE─▶ alice, b2 ─BYE─▶ bob; b1 reboots and reclaims: one CDR
//! ```

use b2bua::config::Cap;
use failover_harness::FailoverHarness;
use sip_message::generators::InDialogMethod;

use crate::departure_support::{bring_up_reactive, placed};
use crate::inflight_resend::{
    kill_serving_node, reboot_and_reclaim, settle_to_one_cdr, split_mut, survivor_of, REPLICATE,
};

#[tokio::test(start_paused = true)]
async fn a_takeover_copy_past_a_cap_answers_500_and_byes_both_legs() {
    let name = "cap-trip-on-a-takeover-copy";
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]).with_worker_tune(|c| {
        c.caps.requests = 0;
    });
    let alice = fh.agent("alice", "127.0.0.1:5060").await;
    let bob = fh.agent("bob", "127.0.0.1:5070").await;
    let (proxy, mut w_b1, mut w_b2) = bring_up_reactive(&mut fh).await;

    let mut dialog = scenario_harness::callflow::establish(&alice, &bob, proxy.addr()).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    fh.advance(REPLICATE).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;

    let mut info = dialog.request(InDialogMethod::Info, None).await;
    info.expect(500).await;
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated + 1,
        "the INFO took the call over on the backup"
    );
    assert_eq!(survivor.metrics().cap_trips_of(Cap::Requests), 1, "the copy's turn tripped");
    assert_eq!(survivor.metrics().reaper_discharged_total(), 0, "the verdict's teardown ran");

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
