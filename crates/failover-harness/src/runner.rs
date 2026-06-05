//! The failover-matrix runner (ADR-0013). Drives one [`Cell`] of the transparent
//! matrix to completion and returns the [`Observation`] + [`TeardownSweep`] for
//! the oracle. A test runs each cell twice — clean baseline (`inject=false`) and
//! with the failover injected (`inject=true`) — and asserts transparency +
//! teardown.
//!
//! The whole topology rides ONE fake clock (`#[tokio::test(start_paused = true)]`)
//! through `FailoverHarness::advance`; every cell routes through the simulated LB
//! and runs with the **genuine limiter logic over the simulated HTTP fabric** —
//! the b2bua's `HttpCallLimiter` client → a real `LimiterServer` + `WindowStore`,
//! but over `SimulatedHttpNetwork` (no socket) on the same fake `Clock::test_at(0)`
//! (no wall-clock). The HTTP round-trips + window/TTL expiry advance with
//! `FailoverHarness::advance`, exactly like the SIP/repl sims, so the
//! limiter-drain invariant is asserted deterministically in every cell (ADR-0013
//! §0). "Genuine" = not a `NoopLimiter`/mock — *not* a real network.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallLimiterEntry, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use call_limiter::{LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use sip_clock::Clock;
use sip_message::generators::InDialogMethod;
use sip_message::message_helpers::{extract_tag, get_header};
use sip_message::types::{SipRequest, SipResponse};

use crate::oracle::{NodeEndState, Observation, TeardownSweep, Who};
use crate::scenario::{Cell, DialogState, Event, Fault, Party, Recovery};
use crate::{FailoverHarness, ReplicatedB2buaSut, WorkerHealth};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";
const LIMITER_ADDR: &str = "10.0.0.1:8080";

fn laddr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

/// The limiter-carrying decision: route every call to bob with a hold on
/// trunk-A, so every cell exercises a genuine limiter hold (acquired/refreshed/
/// released over the simulated HTTP fabric) that must drain to zero.
fn limited_decision() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to("127.0.0.1", 5070);
                r.call_limiter = vec![CallLimiterEntry {
                    id: "trunk-A".into(),
                    limit: 8,
                }];
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

fn limiter_client(http: &SimulatedHttpNetwork) -> Arc<dyn CallLimiter> {
    Arc::new(HttpCallLimiter::new(
        Arc::new(http.clone()),
        laddr(),
        Duration::from_millis(150),
    ))
}

/// `w_pri` / `w_bak` ordinals from the proxy's Record-Route stickiness cookie.
fn pri_bak_from_cookie(rr: &str) -> (String, String) {
    let params = sip_message::message_helpers::parse_uri_params(rr);
    (
        params.get("w_pri").cloned().unwrap_or_default(),
        params.get("w_bak").cloned().unwrap_or_default(),
    )
}

fn req_tags(r: &SipRequest) -> (String, String) {
    let from = get_header(&r.headers, "from").and_then(extract_tag).unwrap_or_default();
    let to = get_header(&r.headers, "to").and_then(extract_tag).unwrap_or_default();
    (from, to)
}

fn resp_cseq(r: &SipResponse) -> String {
    r.cseq.seq.to_string()
}

fn req_cseq(r: &SipRequest) -> String {
    r.cseq.seq.to_string()
}

/// The fixed `target/seq-reports/` artifact root. `CARGO_MANIFEST_DIR` points at
/// `<workspace>/crates/failover-harness`; the workspace `target/` is two levels
/// up. `CARGO_TARGET_DIR` overrides it if set (e.g. a custom target dir in CI).
fn seq_reports_dir() -> std::path::PathBuf {
    if let Ok(t) = std::env::var("CARGO_TARGET_DIR") {
        return std::path::PathBuf::from(t).join("seq-reports");
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/seq-reports")
}

/// The concrete method for a `Generic` event chosen from a seed (the seeded
/// `{method}` pick used when a generated table rotates coverage; hand-written
/// cells set the method explicitly on the `Event`).
pub fn generic_method(seed: u64) -> InDialogMethod {
    match seed % 4 {
        0 => InDialogMethod::Invite,
        1 => InDialogMethod::Update,
        2 => InDialogMethod::Info,
        _ => InDialogMethod::Options,
    }
}

/// Run one matrix cell to completion. `inject` selects baseline (false) vs the
/// failover variant (true). Returns the external observation + the end-state
/// teardown sweep. Panics on any protocol surprise (a failed `expect`/`receive`).
pub async fn run_cell(cell: Cell, inject: bool) -> (Observation, TeardownSweep) {
    let mut obs = Observation::new();
    let name = cell.name();
    let mut fh = FailoverHarness::new(&name, &["b1", "b2"]);

    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    // Lanes registered for the recording (workers receive on their own addrs).
    let b1_lane = fh.agent("b1-lane", B1).await;
    let b2_lane = fh.agent("b2-lane", B2).await;
    drop((b1_lane, b2_lane));

    // Shared limiter server on its own simulated HTTP fabric (survives crashes).
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(WindowStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let _lh: Box<dyn HttpServerHandle> = http.serve(laddr(), server).await.unwrap();

    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let mut w_b1 = fh
        .spawn_worker_limited(
            "b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080),
            limited_decision(), limiter_client(&http),
        )
        .await;
    let mut w_b2 = fh
        .spawn_worker_limited(
            "b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080),
            limited_decision(), limiter_client(&http),
        )
        .await;

    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "[{name}] workers ready at steady state");

    // ── Establish to the cell's safe-point state ─────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    {
        let r = uas.request();
        let (f, t) = req_tags(r);
        obs.req(Who::Bob, "INVITE", &req_cseq(r), &f, &t);
    }
    let rr = get_header(&uas.request().headers, "record-route")
        .expect("b-leg INVITE carries the proxy cookie")
        .to_string();
    let (pri_ord, bak_ord) = pri_bak_from_cookie(&rr);

    // Provisional 180 (gives alice an early dialog).
    uas.respond(180, "Ringing").await;
    {
        let r = call.expect(180).await;
        obs.resp(Who::Alice, 180, &resp_cseq(&r));
    }

    // v1 transparent states: Established (200+ACK) and ConfirmedPreAck (200
    // sent + replicated, ACK pending → ACK routes to the backup). Early is
    // v2-disruptive (a pending INVITE transaction is not replicated, only
    // confirmed call context is — killing the primary mid-INVITE loses it).
    assert_ne!(
        cell.state,
        DialogState::Early,
        "[{name}] Early is v2-disruptive (non-quiescent INVITE txn), not a transparent cell",
    );

    // Bind primary/backup by the cookie's w_pri NOW — needed before the
    // ConfirmedPreAck safe-point (which is pre-ACK). The establish 200/ACK below
    // touches only alice/bob/call, so it does not conflict with these borrows.
    let (primary, backup): (&mut ReplicatedB2buaSut, &mut ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &mut w_b2) } else { (&mut w_b2, &mut w_b1) };
    let primary_ord = pri_ord.clone();
    assert_eq!(bak_ord, backup.ordinal(), "[{name}] cookie w_bak names the backup");

    // 200 OK: the primary establishes + replicates the confirmed call context.
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    {
        let r = call.expect(200).await;
        obs.resp(Who::Alice, 200, &resp_cseq(&r));
    }
    // Bob's dialog view (for callee-initiated in-dialog requests).
    let mut bob_dialog = uas.dialog();

    let mut dialog;
    match cell.state {
        DialogState::Established => {
            dialog = call.ack().await;
            {
                let r = bob.receive("ACK").await;
                let (f, t) = req_tags(r.request());
                obs.req(Who::Bob, "ACK", &req_cseq(r.request()), &f, &t);
            }
            fh.advance(Duration::from_millis(500)).await; // replicate established
            let _ = find_backed_up_ref(backup, &primary_ord).await; // settle
            inject_failover(&mut fh, primary, backup, &primary_ord, &proxy, cell.fault, inject).await;
        }
        DialogState::ConfirmedPreAck => {
            // Safe-point BEFORE the ACK: 200 is sent + replicated, ACK pending.
            fh.advance(Duration::from_millis(500)).await; // replicate confirmed-pre-ack
            let _ = find_backed_up_ref(backup, &primary_ord).await; // settle
            inject_failover(&mut fh, primary, backup, &primary_ord, &proxy, cell.fault, inject).await;
            // The ACK now routes to the backup (primary dead → ACK-exemption
            // does not apply); the backup absorbs it (takeover).
            dialog = call.ack().await;
            {
                let r = bob.receive("ACK").await;
                let (f, t) = req_tags(r.request());
                obs.req(Who::Bob, "ACK", &req_cseq(r.request()), &f, &t);
            }
            fh.advance(Duration::from_millis(500)).await;
        }
        DialogState::Early => unreachable!("asserted above"),
    }

    // Recovery placement differs by category so each cell tests what its name
    // says (ADR-0013 §recovery):
    //  - Nothing / Keepalive: the primary reboots + reclaims BEFORE the (only)
    //    terminating BYE / keepalive tick, so those land on the reclaimed primary
    //    (a clean reclaim of a still-live call — the RebootNoTraffic shape).
    //  - Generic (non-terminating): the event takes the backup over (call LIVE),
    //    THEN the primary reboots + reclaims the LIVE call + hands back (the
    //    exactly-one-owner handback), THEN the call terminates.
    //  - Bye (terminating): the event ends the call on the backup, THEN the
    //    primary reboots — it must reclaim NOTHING (no resurrection).
    let do_reboot = inject
        && matches!(
            cell.recovery,
            Recovery::RebootAfterTakeover | Recovery::RebootNoTraffic
        );
    let reboot_before_event =
        inject && matches!(cell.event, Event::Nothing | Event::Keepalive);
    if reboot_before_event && do_reboot {
        reboot_and_reclaim(&mut fh, primary, backup, &primary_ord, &proxy).await;
    }

    // ── The in-dialog event the backup (or reclaimed primary) processes ──────
    match cell.event {
        Event::Bye(Party::Caller) => {
            let mut bye = dialog.bye().await;
            let mut buas = bob.receive("BYE").await;
            {
                let (f, t) = req_tags(buas.request());
                obs.req(Who::Bob, "BYE", &req_cseq(buas.request()), &f, &t);
            }
            buas.respond(200, "OK").await;
            let r = bye.expect(200).await;
            obs.resp(Who::Alice, 200, &resp_cseq(&r));
            fh.advance(Duration::from_millis(500)).await;
            if !reboot_before_event && do_reboot {
                reboot_and_reclaim(&mut fh, primary, backup, &primary_ord, &proxy).await; // reclaim NOTHING
            }
        }
        Event::Bye(Party::Callee) => {
            let mut bye = bob_dialog.bye().await;
            let mut auas = alice.receive("BYE").await;
            {
                let (f, t) = req_tags(auas.request());
                obs.req(Who::Alice, "BYE", &req_cseq(auas.request()), &f, &t);
            }
            auas.respond(200, "OK").await;
            let r = bye.expect(200).await;
            obs.resp(Who::Bob, 200, &resp_cseq(&r));
            fh.advance(Duration::from_millis(500)).await;
            if !reboot_before_event && do_reboot {
                reboot_and_reclaim(&mut fh, primary, backup, &primary_ord, &proxy).await; // reclaim NOTHING
            }
        }
        Event::Generic { from, method } => {
            drive_generic(&mut obs, &alice, &bob, &mut dialog, &mut bob_dialog, method, from).await;
            fh.advance(Duration::from_millis(500)).await;
            // Reboot + reclaim the LIVE taken-over call + hand back, THEN end it.
            if do_reboot {
                reboot_and_reclaim(&mut fh, primary, backup, &primary_ord, &proxy).await;
            }
            terminating_bye(&mut obs, &alice, &bob, &mut dialog).await;
        }
        Event::Keepalive => {
            // The owner's keepalive OPTIONS probe (after reclaim, if rebooted),
            // refreshing dead-peer detection + the limiter hold. Then terminate.
            keepalive_tick(&mut fh, &alice, &bob, &mut obs).await;
            terminating_bye(&mut obs, &alice, &bob, &mut dialog).await;
        }
        Event::Nothing => {
            // Rebooted+reclaimed above (if any); just terminate.
            terminating_bye(&mut obs, &alice, &bob, &mut dialog).await;
        }
        Event::Cancel => {
            panic!("[{name}] Cancel is v2-disruptive, not a transparent cell");
        }
    }

    fh.advance(Duration::from_millis(500)).await;

    // ── Universal teardown sweep (a few simulated seconds after the end) ──────
    // Let CDR flush + soft releases (limiter) + reverse-deletes drain.
    let mut limiter_total = store.stats().current_total;
    for _ in 0..40 {
        fh.advance(Duration::from_millis(200)).await;
        limiter_total = store.stats().current_total;
        if limiter_total == 0 {
            break;
        }
    }

    let sweep = TeardownSweep {
        nodes: vec![
            node_end_state(primary, &primary_ord, &primary_ord),
            node_end_state(backup, &primary_ord, &bak_ord),
        ],
        cdr_count: primary.cdr_records().len() + backup.cdr_records().len(),
        limiter_total,
    };
    obs.disposition = disposition_of(primary, backup);

    // NOTE: the RFC 3261 "all clean" CSeq audit is not invoked inline here, but it
    // is NOT skipped — `FailoverHarness::drop` runs the same `rfc_audit_findings`
    // hard gate over every cell's recorded trace and panics on any in-dialog CSeq
    // reuse a real UAS would reject. The two faults that once failed every
    // keepalive cell are fixed: `send_request_to_leg` now advances the per-leg
    // dialog CSeq, and the proxy reuses the downstream branch on retransmissions
    // (so a retransmitted keepalive OPTIONS is not seen as a fresh CSeq-N txn).
    let run = if inject { "variant" } else { "baseline" };

    // ── ALWAYS write the unified seq-report (HTML + global.txt) ───────────────
    // One subdir per cell, with a `baseline`/`variant` stem, under the workspace
    // `target/seq-reports/<cell>/`. Reads the recordings NON-consuming (the SIP
    // harness is NOT finished), so it never disturbs the run. No env gating
    // (ADR-0013 / user decision 2).
    let out = seq_reports_dir().join(&name);
    let title = format!("{name} ({run})");
    if let Err(e) = fh.write_unified_report(&out, run, &title, true) {
        eprintln!("[{name}/{run}] could not write unified seq-report: {e}");
    }

    drop((w_b1, w_b2, proxy));
    let _ = fh.repl_report();
    (obs, sweep)
}

/// Inject the failover at the safe-point (variant only — `inject=false` is the
/// clean baseline and does nothing). Kill = abrupt crash; Drain = a grace window
/// then the pod terminates. In both the proxy then routes the dialog to the
/// backup AND the dead pod's endpoint is dropped from the survivor's membership
/// (`simulate_peer_removed`) — the COMPLETE k8s death signal: a real kill removes
/// the StatefulSet endpoint, which the survivor's supervisor turns into an eager
/// `TakeOverPeer`. Without it the harness only modelled proxy reroute (reactive
/// takeover) and never exercised the eager-takeover path that serves quiescent
/// long calls. The reboot re-adds the endpoint ([`reboot_and_reclaim`]).
async fn inject_failover(
    fh: &mut FailoverHarness,
    primary: &mut ReplicatedB2buaSut,
    backup: &ReplicatedB2buaSut,
    primary_ord: &str,
    proxy: &crate::ProxySut,
    fault: Fault,
    inject: bool,
) {
    if !inject {
        return;
    }
    match fault {
        Fault::Kill => {
            // "stop b2b" — the abrupt crash the user wants visible in the diagram.
            fh.mark(primary_ord, None, "crash", "kill (abrupt)");
            primary.crash();
            proxy.set_health(primary_ord, WorkerHealth::Dead);
            // k8s drops the dead pod's endpoint → survivor eager-takes-over.
            backup.simulate_peer_removed(primary_ord);
            fh.advance(Duration::from_millis(300)).await;
        }
        Fault::Drain => {
            fh.mark(primary_ord, None, "failover", "drain begin");
            proxy.set_health(primary_ord, WorkerHealth::Draining);
            fh.advance(Duration::from_secs(6)).await;
            // "stop b2b" — the pod terminates after its grace window.
            fh.mark(primary_ord, None, "crash", "drain → terminate");
            primary.crash();
            proxy.set_health(primary_ord, WorkerHealth::Dead);
            backup.simulate_peer_removed(primary_ord);
            fh.advance(Duration::from_millis(300)).await;
        }
    }
}

/// Reboot the primary EMPTY at a higher gen, mark it alive, drive re-hydration to
/// ready, then drive the go-active reclaim + handback window.
async fn reboot_and_reclaim(
    fh: &mut FailoverHarness,
    primary: &mut ReplicatedB2buaSut,
    backup: &ReplicatedB2buaSut,
    primary_ord: &str,
    proxy: &crate::ProxySut,
) {
    // "restart b2b" — the rebooted pod re-joins empty at a higher gen.
    fh.mark(primary_ord, None, "reboot", "restart empty, higher gen");
    primary.reboot().await;
    proxy.set_health(primary_ord, WorkerHealth::Alive);
    // k8s re-publishes the restarted pod's endpoint (a StatefulSet restart is
    // observed as Removed-then-Added) → the survivor re-spawns its puller to the
    // peer, the channel the X11 `Deactivate` handback rides back on. Pairs with
    // the `simulate_peer_removed` the kill drove in `inject_failover`.
    backup.simulate_peer_added(primary_ord);
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if primary.is_ready() {
            break;
        }
    }
    assert!(primary.is_ready(), "rebooted primary {primary_ord} became ready");
    // ReclaimAll + the ~5 s Deactivate handback broadcast + puller reconnect.
    fh.advance(Duration::from_secs(10)).await;
}

/// Drive a non-terminating in-dialog request from `from`, fully (offer/answer for
/// re-INVITE/UPDATE; plain request/200 for INFO/OPTIONS).
async fn drive_generic(
    obs: &mut Observation,
    alice: &scenario_harness::Agent,
    bob: &scenario_harness::Agent,
    dialog: &mut scenario_harness::Dialog,
    bob_dialog: &mut scenario_harness::Dialog,
    method: InDialogMethod,
    from: Party,
) {
    let needs_answer = matches!(method, InDialogMethod::Invite | InDialogMethod::Update);
    let m = method_name(method);
    match from {
        Party::Caller => {
            let sdp = needs_answer.then_some(OFFER);
            let mut tx = dialog.request(method, sdp).await;
            let mut peer = bob.receive(m).await;
            {
                let (f, t) = req_tags(peer.request());
                obs.req(Who::Bob, m, &req_cseq(peer.request()), &f, &t);
            }
            if needs_answer {
                peer.respond(200, "OK").with_sdp(ANSWER).await;
            } else {
                peer.respond(200, "OK").await;
            }
            let r = tx.expect(200).await;
            obs.resp(Who::Alice, 200, &resp_cseq(&r));
            if method == InDialogMethod::Invite {
                dialog.ack(Some(ANSWER)).await;
                let a = bob.receive("ACK").await;
                let (f, t) = req_tags(a.request());
                obs.req(Who::Bob, "ACK", &req_cseq(a.request()), &f, &t);
            }
        }
        Party::Callee => {
            let sdp = needs_answer.then_some(OFFER);
            let mut tx = bob_dialog.request(method, sdp).await;
            let mut peer = alice.receive(m).await;
            {
                let (f, t) = req_tags(peer.request());
                obs.req(Who::Alice, m, &req_cseq(peer.request()), &f, &t);
            }
            if needs_answer {
                peer.respond(200, "OK").with_sdp(ANSWER).await;
            } else {
                peer.respond(200, "OK").await;
            }
            let r = tx.expect(200).await;
            obs.resp(Who::Bob, 200, &resp_cseq(&r));
            if method == InDialogMethod::Invite {
                bob_dialog.ack(Some(ANSWER)).await;
                let a = alice.receive("ACK").await;
                let (f, t) = req_tags(a.request());
                obs.req(Who::Alice, "ACK", &req_cseq(a.request()), &f, &t);
            }
        }
    }
}

/// Advance past the keepalive interval and service the AS's own OPTIONS probe on
/// each leg (the owner keepalives both legs; answering refreshes dead-peer
/// detection AND the call-limiter hold). Per-UA capture means the relative order
/// of the two probes is irrelevant to the differential.
async fn keepalive_tick(
    fh: &mut FailoverHarness,
    alice: &scenario_harness::Agent,
    bob: &scenario_harness::Agent,
    obs: &mut Observation,
) {
    // PRODUCTION keepalive interval (300 s; the harness runs the real cadence).
    // The deadline is one interval out from whenever the keepalive was last
    // (re-)armed: from *establish* in the baseline, but from *reclaim* in the
    // reboot variant (`reclaim_into_live` re-arms a fresh interval) — so the two
    // runs start the tick at different absolute times and a fixed `advance(300s)`
    // would overshoot the variant's deadline + its 5 s reap and tear the call
    // down (the CLAUDE.md keepalive hazard, and exactly the long-call-on-reboot
    // loss the endurance run flagged). Instead poll-advance toward whichever
    // deadline applies in sub-reap (2 s) steps, draining each leg's OPTIONS the
    // instant it is queued so every leg is answered strictly inside its 5 s
    // dead-peer window. Both legs are probed together (one keepalive fires
    // OPTIONS to every peered leg), so they surface in the same step.
    let tol = ["INVITE", "ACK", "UPDATE", "INFO", "BYE"];
    let mut a_txn = None;
    let mut b_txn = None;
    // Pump in 2 s steps (< the 5 s dead-peer reap) toward whichever deadline
    // applies, stopping the instant BOTH legs' OPTIONS are queued. Bound at one
    // interval + margin so a call that is NOT being kept alive (a regressed
    // reclaim that never re-armed the keepalive) fails the `expect` below rather
    // than hanging — the cell keeps its teeth.
    let serviced = fh
        .pump_until(Duration::from_secs(2), Duration::from_secs(340), async || {
            if a_txn.is_none() {
                a_txn = alice.try_receive_tolerating("OPTIONS", &tol).await;
            }
            if b_txn.is_none() {
                b_txn = bob.try_receive_tolerating("OPTIONS", &tol).await;
            }
            a_txn.is_some() && b_txn.is_some()
        })
        .await;
    assert!(serviced, "the AS keepalive OPTIONS reached BOTH legs within an interval");

    // Answer both legs BEFORE advancing again — each cancels its leg's 5 s reap.
    let mut a = a_txn.expect("alice received the AS keepalive OPTIONS within an interval");
    {
        let (f, t) = req_tags(a.request());
        obs.req(Who::Alice, "OPTIONS", &req_cseq(a.request()), &f, &t);
    }
    let mut b = b_txn.expect("bob received the AS keepalive OPTIONS within an interval");
    {
        let (f, t) = req_tags(b.request());
        obs.req(Who::Bob, "OPTIONS", &req_cseq(b.request()), &f, &t);
    }
    a.respond(200, "OK").await;
    b.respond(200, "OK").await;
    fh.advance(Duration::from_millis(300)).await;
}

/// The always-terminate final BYE (caller-initiated) every non-terminating cell
/// appends so the universal teardown sweep holds.
async fn terminating_bye(
    obs: &mut Observation,
    _alice: &scenario_harness::Agent,
    bob: &scenario_harness::Agent,
    dialog: &mut scenario_harness::Dialog,
) {
    let mut bye = dialog.bye().await;
    // Tolerate stray keepalive OPTIONS retransmits still in the queues (the
    // Keepalive category advances a full interval in one step, which fires the
    // OPTIONS *and* its retransmit before the 200 lands).
    let mut buas = bob.receive_tolerating("BYE", &["OPTIONS"]).await;
    {
        let (f, t) = req_tags(buas.request());
        obs.req(Who::Bob, "BYE", &req_cseq(buas.request()), &f, &t);
    }
    buas.respond(200, "OK").await;
    let r = bye.expect_tolerating(200, &["OPTIONS"]).await;
    obs.resp(Who::Alice, 200, &resp_cseq(&r));
}

fn method_name(m: InDialogMethod) -> &'static str {
    match m {
        InDialogMethod::Invite => "INVITE",
        InDialogMethod::Update => "UPDATE",
        InDialogMethod::Info => "INFO",
        InDialogMethod::Options => "OPTIONS",
        InDialogMethod::Bye => "BYE",
        InDialogMethod::Notify => "NOTIFY",
        InDialogMethod::Message => "MESSAGE",
        InDialogMethod::Prack => "PRACK",
        InDialogMethod::Refer => "REFER",
    }
}

fn node_end_state(node: &ReplicatedB2buaSut, primary_ord: &str, ordinal: &str) -> NodeEndState {
    let alive = node.is_ready() || node.active_calls() > 0 || node.lock_count() > 0;
    NodeEndState {
        ordinal: ordinal.to_string(),
        alive,
        active_calls: node.active_calls(),
        lock_count: node.lock_count(),
        residual_pri: node.scan_primary(primary_ord),
        residual_bak: node.scan_backed_up(primary_ord),
    }
}

fn disposition_of(primary: &ReplicatedB2buaSut, backup: &ReplicatedB2buaSut) -> String {
    // The externally-meaningful outcome: whichever node wrote the end CDR.
    let mut cdrs = primary.cdr_records();
    cdrs.extend(backup.cdr_records());
    if cdrs.is_empty() {
        "no-cdr".into()
    } else {
        "terminated".into()
    }
}

/// The callRef the backup holds for `primary` in `bak:{primary}` (the replicated
/// call), retrying while replication settles.
async fn find_backed_up_ref(backup: &ReplicatedB2buaSut, primary: &str) -> String {
    for _ in 0..50 {
        let refs = backup.scan_backed_up(primary);
        if let Some(rf) = refs.into_iter().next() {
            return rf;
        }
        tokio::task::yield_now().await;
    }
    panic!("no backed-up call ref in bak:{primary} on the backup");
}
