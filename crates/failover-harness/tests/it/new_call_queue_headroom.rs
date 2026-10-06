//! **Normal new INVITEs stop below the queue cap; a taken-over call's first
//! in-dialog request and an emergency INVITE reach the full cap.** A backup
//! opens a taken-over call's queue only when the call's first in-dialog
//! request lands there, so that request competes with new INVITEs for the
//! node's last live queues. A normal new INVITE opens a queue only below
//! `per_call_queue_cap` less the new-call headroom; every other event, an
//! emergency INVITE included, opens one up to the cap.
//!
//! ```text
//!   call A establishes            primary P, replica on backup Q
//!   proxy sees P dead             (P keeps running: a misroute, ADR-0014)
//!   calls N1, N2 establish on Q   Q holds 2 queues = cap 4 − headroom 2
//!   new normal INVITE             503 at the headroom, bob never reached
//!   emergency INVITE              admitted on Q: 3 queues
//!   alice BYE on call A           Q takes the call over: 4 queues, BYE 200
//!   N1, N2 and the emergency call end
//! ```

use std::time::Duration;

use b2bua::admission::Class;
use b2bua::new_calls::{NewCallCounts, Refusal};
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness,
    ReplicatedB2buaSut, WorkerHealth,
};
use scenario_harness::{Agent, Dialog};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const EMERGENCY: (&str, &str) = ("Resource-Priority", "esnet.0");

const ALICE: &str = "127.0.0.1:5060";
const CAROL: &str = "127.0.0.1:5061";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

const QUEUE_CAP: usize = 4;
/// Half the cap: normal new INVITEs stop at 2 live queues.
const HEADROOM_PERCENT: u8 = 50;

/// A confirmed call from `caller` through the proxy, emergency when
/// `emergency`; returns the caller's dialog and the primary's ordinal.
async fn establish(
    caller: &Agent,
    bob: &Agent,
    proxy: std::net::SocketAddr,
    emergency: bool,
) -> (Dialog, String) {
    let mut invite = caller.invite(bob).with_sdp(OFFER);
    if emergency {
        invite = invite.with_header(EMERGENCY.0, EMERGENCY.1);
    }
    let mut call = invite.through(proxy).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (primary, _) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    (dialog, primary)
}

/// The per-call queues `w` has open.
fn open_queues(w: &ReplicatedB2buaSut) -> u64 {
    w.metrics().creations_total() - w.metrics().removals_total()
}

#[tokio::test(start_paused = true)]
async fn a_taken_over_calls_first_request_and_an_emergency_invite_pass_the_new_call_headroom() {
    let mut fh =
        FailoverHarness::new("new-call-queue-headroom", &["b1", "b2"]).with_worker_tune(|c| {
            c.per_call_queue_cap = QUEUE_CAP;
            c.new_call_queue_headroom_percent = HEADROOM_PERCENT;
        });
    let alice = fh.agent("alice", ALICE).await;
    let carol = fh.agent("carol", CAROL).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let w_b1 =
        fh.spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    let w_b2 =
        fh.spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080)).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");

    // ── call A, replicated to its backup ────────────────────────────────────
    let (mut dialog_a, primary) = establish(&alice, &bob, proxy.addr(), false).await;
    fh.advance(Duration::from_millis(500)).await;
    let (w_p, w_q) = if primary == "b1" { (&w_b1, &w_b2) } else { (&w_b2, &w_b1) };
    let ref_a = w_p.scan_primary(&primary).pop().expect("the primary stores call A");
    assert!(w_q.scan_backed_up(&primary).contains(&ref_a), "A is backed up on Q");

    // The proxy reads P dead and routes everything to Q; P keeps running and
    // owning call A.
    proxy.set_health(&primary, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(200)).await;

    // ── Q fills to the headroom with new calls ──────────────────────────────
    let mut normals = Vec::new();
    for _ in 0..2 {
        let (dialog, on) = establish(&carol, &bob, proxy.addr(), false).await;
        assert_eq!(on, w_q.ordinal(), "a new call lands on Q");
        normals.push(dialog);
    }
    assert_eq!(open_queues(w_q), 2, "Q holds the queues of its two calls only");

    let cap_sheds = || {
        let counts = NewCallCounts::compose(
            w_q.metrics().new_calls(),
            Default::default(),
            Default::default(),
        );
        counts.rejected(Refusal::CapShed, Class::Normal)
    };
    let shed = cap_sheds();
    let mut refused = carol.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    refused.expect(503).await;
    assert_eq!(cap_sheds(), shed + 1, "refused at the headroom");
    assert_eq!(open_queues(w_q), 2, "the refused INVITE opened no queue");

    let (mut urgent, on) = establish(&carol, &bob, proxy.addr(), true).await;
    assert_eq!(on, w_q.ordinal(), "the emergency call lands on Q");
    assert_eq!(open_queues(w_q), 3, "the emergency INVITE opened a queue past the headroom");

    // ── call A's first request on Q opens its queue past the headroom ───────
    let opened = w_q.metrics().creations_total();
    scenario_harness::callflow::hangup(&mut dialog_a, &bob).await;
    assert_eq!(w_q.metrics().creations_total(), opened + 1, "the takeover opened a queue on Q");

    for dialog in &mut normals {
        scenario_harness::callflow::hangup(dialog, &bob).await;
    }
    scenario_harness::callflow::hangup(&mut urgent, &bob).await;

    let _ = fh
        .settle_terminal(async || {
            w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&ref_a).await
                && !w_b2.holds_any_trace(&ref_a).await
        })
        .await;
    fh.linger_peers(&[&alice, &carol, &bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&[&w_b1, &w_b2], &ref_a), 1, "exactly one CDR for call A");
    assert_call_fully_released(&[&w_b1, &w_b2], &ref_a).await;
    let cdrs = w_b1.cdr_records().len() + w_b2.cdr_records().len();
    assert_eq!(cdrs, 4, "one CDR per established call, none for the refused INVITE");
    fh.assert_full_rfc_clean("new-call-queue-headroom");
    drop(proxy);
}
