//! **Two proxy instances holding opposite views of a live primary**, at sync
//! level `applied`. Instances A and B serve one address; each party's flow
//! reaches one of them. The caller's flow and the backup's reach A; the
//! callee's flow and the primary's reach B, so each instance relays both
//! directions of the call and neither holds all of it. A reads the primary
//! dead and tells the backup; B reads it alive. The replication stream stays
//! open, so the told verdict takes nothing over (ADR-0047 D1).
//!
//! The callee's BYE crosses B to the primary while the caller's re-INVITE
//! crosses A to the backup, whose copy takes the call over and relays the
//! round to the callee. The primary ends the call. The callee, whose dialog
//! its BYE ended, answers the relayed re-INVITE `481`; the copy then ends the
//! round `487` to the caller and sends the callee a BYE, which it answers
//! `481` too. The caller's `200` to the primary's BYE crosses A, which hands
//! a response bound to a worker it reads dead to the backup, so the primary's
//! BYE runs to its timeout while the caller answers each repeat.
//!
//! Judged as a split (ADR-0055 §11): no leak, the call fully over on both
//! nodes, one CDR, no copy left serving. The RFC findings two acting copies
//! produce are accepted by the split's rows and listed: `W-split-ended-dialog`
//! twice, the copy's re-INVITE and BYE into the callee's dialog its own BYE
//! ended, each closed by the `481` it draws.
//!
//! Beyond the failover sweep: two front proxy instances with split worker views.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::http::HttpCallLimiter;
use b2bua::limiter::CallLimiter;
use b2bua::repl::SyncLevel;
use call::LimiterEntry;
use call_limiter::{CallStore, LimiterMetrics, LimiterServer};
use failover_harness::{
    assert_call_fully_over, worker_ordinals, FailoverHarness, Incident, Instance, Tier,
    WorkerHealth, LEASE_OUTLIVING_THE_REPLICA_TTL,
};
use http_net::{HttpServerHandle, HttpTransport};
use scenario_harness::{Agent, Inbound};
use sip_clock::Clock;
use sip_message::generators::InDialogMethod;
use sip_net_testkit::RecordedSipEntry;

use crate::call_terminate_on_backup::{ANSWER, OFFER};

const NAME: &str = "split-proxy-views";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const VIP: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";
const LIMITER: &str = "10.0.0.1:8080";

/// How long the caller keeps answering the primary's BYE: past its Timer F.
const BYE_TIMEOUT_OUTLIVED: Duration = Duration::from_secs(40);

fn addr(at: &str) -> SocketAddr {
    at.parse().unwrap()
}

/// Every call to the callee, holding one limiter entry.
fn decision() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to("127.0.0.1", 5070);
                r.call_limiter = vec![LimiterEntry { id: "trunk-A".into(), limit: 8 }];
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// Whether `method` from `from` was delivered on `lane` and relayed from it
/// to `to`.
fn crossed(
    entries: &[RecordedSipEntry],
    method: &str,
    from: SocketAddr,
    lane: &str,
    to: SocketAddr,
) -> bool {
    let is = |e: &RecordedSipEntry| e.raw.starts_with(format!("{method} ").as_bytes());
    let arrived =
        entries.iter().any(|e| is(e) && e.from == from && e.to_lane.as_deref() == Some(lane));
    let relayed =
        entries.iter().any(|e| is(e) && e.from_lane.as_deref() == Some(lane) && e.to == to);
    arrived && relayed
}

/// Advance `window`, each of `parties` answering every repeat of a request
/// it already answered (below this API) and taking nothing new.
async fn answer_repeats(fh: &FailoverHarness, parties: &[&Agent], window: Duration) {
    let step = Duration::from_millis(200);
    let mut elapsed = Duration::ZERO;
    while elapsed < window {
        fh.advance(step).await;
        elapsed += step;
        for party in parties {
            match party.take_queued_inbound().await {
                Some(Inbound::Request(txn)) => {
                    panic!("{} took a new {}", party.name(), txn.request().method())
                }
                Some(Inbound::Response(r)) => {
                    panic!("{} took a stray {}", party.name(), r.status())
                }
                None => {}
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_bye_through_the_instance_reading_the_primary_alive_crosses_a_reinvite_to_the_copy() {
    let mut fh = FailoverHarness::new(NAME, &["b1", "b2"]).with_sync_level(SyncLevel::Applied);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let http = http_net_testkit::SimulatedHttpNetwork::new();
    let store = Arc::new(CallStore::new(LEASE_OUTLIVING_THE_REPLICA_TTL, Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let _lh: Box<dyn HttpServerHandle> = http.serve(addr(LIMITER), server).await.unwrap();
    let limiter = || -> Arc<dyn CallLimiter> {
        let transport = Arc::new(http.clone());
        Arc::new(HttpCallLimiter::new(transport, addr(LIMITER), Duration::from_millis(150)))
    };
    let pair = fh.spawn_proxy_pair(VIP, &[("b1", addr(B1)), ("b2", addr(B2))]).await;
    pair.assign(addr(ALICE), Instance::A);
    pair.assign(addr(BOB), Instance::B);
    let mut workers = Vec::new();
    for (ordinal, at, peer) in [("b1", B1, "b2"), ("b2", B2, "b1")] {
        let worker = fh
            .spawn_worker_limited(
                ordinal,
                ordinal,
                at,
                &[peer],
                ("127.0.0.1", 5070),
                ("127.0.0.1", 5080),
                decision(),
                limiter(),
            )
            .await;
        workers.push(worker);
    }
    let (w_b2, w_b1) = (workers.pop().unwrap(), workers.pop().unwrap());
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "workers ready at steady state");

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(pair.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (primary_ord, _) = worker_ordinals(uas.request());
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut bob_dialog = uas.dialog();
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    fh.advance(Duration::from_millis(500)).await;
    let (primary, backup) = if primary_ord == "b1" { (&w_b1, &w_b2) } else { (&w_b2, &w_b1) };
    let call_ref =
        backup.scan_one_backed_up(&primary_ord).await.expect("the backup holds a replica");
    pair.assign(primary.sip_addr(), Instance::B);
    pair.assign(backup.sip_addr(), Instance::A);

    pair.a.set_health(&primary_ord, WorkerHealth::Dead);
    fh.strike(Incident::Split);
    fh.advance(Duration::from_millis(200)).await;
    assert_eq!(pair.b.health(&primary_ord), Some(WorkerHealth::Alive), "B reads it alive");
    assert_eq!(backup.metrics().dead_verdicts_taken_total(), 1, "A's verdict is told");
    assert!(!backup.serves(&call_ref), "a told verdict takes nothing over");

    let mut bye = bob_dialog.bye().await;
    let mut round = dialog.request(InDialogMethod::Invite, Some(OFFER)).await;
    alice.receive("BYE").await.respond(200, "OK").await;
    round.expect(100).await;
    bye.expect(200).await;
    bob.receive("INVITE").await.respond(481, "Call/Transaction Does Not Exist").await;
    round.expect(487).await;
    bob.receive("BYE").await.respond(481, "Call/Transaction Does Not Exist").await;
    assert_eq!(backup.metrics().repl_takeover_hydrated_total(), 1, "the copy took the round");

    let entries = fh.sip_entries();
    let (lane_a, lane_b) = (format!("{VIP}#proxy-a"), format!("{VIP}#proxy-b"));
    assert!(crossed(&entries, "BYE", addr(BOB), &lane_b, primary.sip_addr()), "BYE: B, primary");
    let to_copy = crossed(&entries, "INVITE", addr(ALICE), &lane_a, backup.sip_addr());
    assert!(to_copy, "the caller's re-INVITE crosses A to the backup");

    answer_repeats(&fh, &[&alice, &bob], BYE_TIMEOUT_OUTLIVED).await;
    let drained = fh
        .settle_terminal(async || {
            store.stats().current_total == 0
                && w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(&call_ref).await
                && !w_b2.holds_any_trace(&call_ref).await
        })
        .await;
    assert!(drained, "the call drains on both nodes");
    fh.linger_peers(&[&alice, &bob], Duration::from_secs(3)).await;
    assert_call_fully_over(&[&w_b1, &w_b2], &call_ref, &store).await;
    assert_eq!(fh.tier(), Some(Tier::Degraded), "a split is never Guaranteed");
    let fired: Vec<String> =
        fh.fired_rows().iter().map(|f| format!("{} {}", f.row.id, f.what)).collect();
    let accepted = |method: &str| {
        let finding = format!("W-split-ended-dialog [bob] dialog-ended: {method} ");
        fired.iter().any(|f| f.starts_with(&finding))
    };
    let listed = fired.len() == 2 && accepted("INVITE") && accepted("BYE");
    assert!(listed, "the two copies' findings: {fired:?}");
}
