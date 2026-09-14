//! **A withdrawn primary answers a call inside the window its process outlives
//! its endpoint.**
//!
//! A force-deleted node loses its endpoint and its process at different
//! instants: membership drops the ordinal at once, the process takes the
//! SIGTERM as a drain and keeps serving its live calls until the SIGKILL
//! lands. A same-ordinal replacement comes up inside that window, bootstraps
//! and reclaims the ringing snapshot from the survivor.
//!
//! ```text
//!   ringing · elder serves · survivor holds the ringing replica
//!   t0        endpoint withdrawn + SIGTERM
//!             A, C: registry + peers drop the ordinal
//!             B:    the endpoint stays in the slice as terminating — the proxy
//!                   departs it, the peers keep pulling it (ADR-0031 case 1)
//!   +0.5 s    A, C: a replacement of the SAME ordinal comes up alongside and
//!                   reclaims the ringing copy
//!   +1.1 s    the callee answers 200 — the elder is still bound and still serving
//!   +1.8 s    A, C: the replacement's address is published, then judged alive
//!   +2.1 s    A: SIGKILL · C: dead since t0
//!   +5 s      B: the drain returns, the endpoint leaves the slice (peers park it),
//!                the process exits; only then the replacement comes up, reclaims
//!                the answered copy and is published
//!   INVITE+30 s  the ring deadline fires on the reclaimed copy
//! ```
//!
//! The proxy identifies the worker a response belongs to by the Via **sent-by**
//! — the address the registry published for it. An address that left the worker
//! set keeps resolving as `Dead` for Timer H, so the callee's 2xx reverse-fails
//! to the cookie's backup instead of returning to the incarnation that sent the
//! INVITE: the survivor's takeover copy answers the caller, the caller's ACK
//! follows the same cookie to it, and the reverse-flush fold hands the answer to
//! the replacement's reclaimed copy. Every owner then reads the call as
//! answered, so no ring deadline authors a second final on an INVITE server
//! transaction the caller already ACKed (RFC 3261 §17.2.1, §13.3.1.4).
//!
//! The three cases differ in how the elder leaves: A forces the removal (the
//! endpoint leaves the slice, the peers park it, SIGKILL mid-window), B removes
//! it gracefully (the endpoint stays in the slice as `terminating`, the peers
//! keep pulling it through its drain, no kill at all — ADR-0031 case 1), C makes
//! the process die WITH its endpoint. The answer reaches the caller in all three.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallTreatment, NewCallResponse, RejectDecision, ScriptedDecisionEngine,
};
use b2bua::limiter::NoopLimiter;
use call::{CallBodyCodec, LegState, MsgpackCodec, TimerType};
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, Belief, FailoverHarness,
    PartitionRole, ProxySut, ReplicatedB2buaSut, WorkerHealth, RULE_CSEQ_IN_DIALOG_ORDER,
};
use scenario_harness::Agent;
use sip_message::SipMessage;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

const BAK: PartitionRole = PartitionRole::Backup;

/// The deployed ring deadline: armed at route time, far enough out that the
/// replacement bootstraps and reclaims long before it fires.
const NO_ANSWER_SEC: i64 = 30;

/// The orchestrator's timeline, relative to the withdrawal (ms). Each is a
/// target: an advance lands on the next 100 ms pump boundary at or after it, and
/// the report carries the instants the run actually hit.
const REPLACEMENT_AT: i64 = 400;
const ANSWER_AT: i64 = 1_000;
const READMIT_AT: i64 = 1_800;
const SIGKILL_AT: i64 = 2_000;
const ALIVE_AT: i64 = 2_200;

/// The shutdown grace the SIGTERM starts — longer than the force-delete window,
/// so case A's kill lands inside the drain and case B's drain runs it out.
const GRACE: Duration = Duration::from_secs(5);

/// The drain's bounds: the same 5 s ceiling, with the caught-up exit's 1 s floor
/// (ADR-0031 D2) — a request routed before the withdrawal is still served.
const BOUNDS: failover_harness::DrainBounds =
    failover_harness::DrainBounds { grace: GRACE, floor: Duration::from_millis(1000) };

/// The deployed shape: a failover-capable route (callback context) arming the
/// per-b-leg `NoAnswer`, whose `no_answer_timeout` failure consult answers with
/// a `480` reject.
fn no_answer_decision() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.no_answer_timeout_sec = Some(NO_ANSWER_SEC);
                r.callback_context = Some("withdrawn-window-ctx".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_| {
                CallTreatment::Reject(RejectDecision {
                    reject_code: 480,
                    reject_reason: Some("Temporarily Unavailable".into()),
                    update_headers: None,
                    service_ext: Default::default(),
                    label: None,
                })
            })
            .build(),
    )
}

struct Cluster {
    fh: FailoverHarness,
    alice: Agent,
    bob: Agent,
    proxy: ProxySut,
    w_b1: ReplicatedB2buaSut,
    w_b2: ReplicatedB2buaSut,
}

/// Two replicating workers behind the proxy, both ready, both sending their
/// b-leg back through the proxy — so a callee's response travels the proxy's
/// response path, where a worker is identified by its Via sent-by.
async fn spawn_cluster(name: &str) -> Cluster {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;

    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let w_b1 = fh
        .spawn_worker_limited(
            "b1",
            "b1",
            B1,
            &["b2"],
            ("127.0.0.1", 5070),
            ("127.0.0.1", 5080),
            no_answer_decision(),
            Arc::new(NoopLimiter),
        )
        .await;
    let w_b2 = fh
        .spawn_worker_limited(
            "b2",
            "b2",
            B2,
            &["b1"],
            ("127.0.0.1", 5070),
            ("127.0.0.1", 5080),
            no_answer_decision(),
            Arc::new(NoopLimiter),
        )
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both ready at steady state");
    Cluster { fh, alice, bob, proxy, w_b1, w_b2 }
}

/// The first replicated ref in `bak:{primary}` on the survivor.
async fn find_backed_up_ref(backup: &ReplicatedB2buaSut, primary: &str) -> String {
    for _ in 0..50 {
        if let Some(rf) = backup.scan_one_backed_up(primary).await {
            return rf;
        }
        tokio::task::yield_now().await;
    }
    panic!("no backed-up call ref found in bak:{primary} on the survivor");
}

/// The absolute deadline of the survivor's replica per-b-leg `NoAnswer` entry.
async fn ring_deadline(backup: &ReplicatedB2buaSut, primary: &str, call_ref: &str) -> i64 {
    let body = backup.get(BAK, primary, call_ref).await.expect("replica body present");
    let call: call::Call = MsgpackCodec::new().decode(&body).expect("replica body decodes");
    assert_ne!(call.a_leg.state, LegState::Confirmed, "the survivor holds the ringing a-leg");
    call.timers
        .iter()
        .find(|t| t.timer_type == TimerType::NoAnswer)
        .map(|t| t.fire_at)
        .expect("the replica carries the route-time NoAnswer")
}

/// Advance to `t0 + offset_ms` on the virtual clock (a no-op when already past
/// it — each `advance` carries a trailing 100 ms chunk).
async fn advance_to(fh: &FailoverHarness, t0: i64, offset_ms: i64) {
    let delta = t0 + offset_ms - fh.now_ms();
    if delta > 0 {
        fh.advance(Duration::from_millis(delta as u64)).await;
    }
}

/// Every recorded datagram whose start-line begins with `prefix` and whose CSeq
/// names `method`, as `(from, to, delivered)` in wire order.
fn hops(fh: &FailoverHarness, prefix: &str, method: &str) -> Vec<(SocketAddr, SocketAddr, bool)> {
    fh.sip_entries()
        .into_iter()
        .filter(|e| {
            let text = String::from_utf8_lossy(&e.raw);
            text.starts_with(prefix)
                && text.lines().any(|l| {
                    let l = l.to_ascii_lowercase();
                    l.starts_with("cseq:") && l.ends_with(&method.to_ascii_lowercase())
                })
        })
        .map(|e| (e.from, e.to, e.delivered))
        .collect()
}

/// The cluster-side destinations [`hops`] recorded, in wire order — the columns
/// a message actually landed on.
fn relayed_to(hops: &[(SocketAddr, SocketAddr, bool)]) -> Vec<String> {
    hops.iter()
        .map(
            |(_, to, delivered)| {
                if *delivered {
                    to.to_string()
                } else {
                    format!("{to} (undelivered)")
                }
            },
        )
        .collect()
}

/// What the ring deadline did on the wire: every INVITE final the caller took
/// across the window, and the CANCELs the callee's INVITE drew.
struct DeadlineCrossing {
    finals_to_alice: Vec<u16>,
    cancels_to_bob: usize,
}

impl DeadlineCrossing {
    /// The first final the caller took at the deadline, if any.
    fn stray_final(&self) -> Option<u16> {
        self.finals_to_alice.first().copied()
    }
}

/// Cross the ring deadline. The callee's INVITE transaction took its 2xx, so a
/// CANCEL reaching it draws a 481 (RFC 3261 §9.2); every non-2xx final the
/// caller takes is hop-ACKed, as a real UAC's transaction layer does (§17.1.1.3).
async fn cross_deadline(
    fh: &FailoverHarness,
    call: &scenario_harness::ClientInvite,
    alice: &Agent,
    bob: &Agent,
    fire_at: i64,
) -> DeadlineCrossing {
    let mut out = DeadlineCrossing { finals_to_alice: Vec::new(), cancels_to_bob: 0 };
    while fh.now_ms() < fire_at + 2_000 {
        fh.advance(Duration::from_secs(1)).await;
        // A 2xx the elder keeps retransmitting draws a re-ACK on the b-leg, so
        // ACK is tolerated here alongside the keepalive.
        while let Some(mut cancel) = bob.try_receive_tolerating("CANCEL", &["OPTIONS", "ACK"]).await
        {
            out.cancels_to_bob += 1;
            cancel.respond(481, "Call/Transaction Does Not Exist").await;
        }
        while let Some(msg) = alice.take_queued().await {
            match msg {
                SipMessage::Response(r) if r.cseq().method().as_str() == "INVITE" => {
                    if r.status() >= 300 {
                        call.ack_non_2xx(&r).await.expect("the caller hop-ACKs the final");
                    }
                    out.finals_to_alice.push(r.status());
                }
                SipMessage::Response(_) => {}
                SipMessage::Request(r) => panic!("unexpected {} toward alice", r.method()),
            }
        }
    }
    out
}

/// The callee answers at `t0 + ANSWER_AT`, inside the elder's window: the 2xx
/// must reach the caller whichever incarnation sent the INVITE, and her ACK
/// follows. Marks the route the answer took and who serves the call after it;
/// returns the caller's established dialog.
#[allow(clippy::too_many_arguments)]
async fn answer_inside_window(
    fh: &mut FailoverHarness,
    pri_ord: &str,
    t0: i64,
    uas: &mut scenario_harness::ServerTxn,
    call: &mut scenario_harness::ClientInvite,
    drain: Option<&mut failover_harness::PendingDrain>,
    elder: &ReplicatedB2buaSut,
    replacement: Option<&ReplicatedB2buaSut>,
    survivor: &ReplicatedB2buaSut,
    call_ref: &str,
) -> scenario_harness::Dialog {
    advance_to(fh, t0, ANSWER_AT).await;
    fh.mark(pri_ord, None, "answer", "the callee answers inside the window");
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    fh.advance(Duration::from_millis(300)).await;
    if let Some(d) = drain {
        d.poll();
    }
    // Read the answer's route off the recording rather than waiting on the
    // caller: a lost answer must cost no clock.
    let alice_addr: SocketAddr = ALICE.parse().unwrap();
    let answered = hops(fh, "SIP/2.0 200", "INVITE")
        .iter()
        .any(|(_, to, delivered)| *delivered && *to == alice_addr);
    assert!(answered, "the callee's 2xx reached the caller");
    call.expect(200).await;
    let dialog = call.ack().await;
    fh.advance(Duration::from_millis(200)).await;
    fh.mark(
        pri_ord,
        None,
        "answer route",
        &format!(
            "2xx relayed to {:?}; caller ACK relayed to {:?}",
            relayed_to(&hops(fh, "SIP/2.0 200", "INVITE")),
            relayed_to(&hops(fh, "ACK ", "ACK")),
        ),
    );
    fh.mark(
        pri_ord,
        None,
        "after answer",
        &format!(
            "caller answered={answered} elder({}) serves={} replacement={} survivor serves={}",
            elder.sip_addr(),
            elder.serves(call_ref),
            match replacement {
                Some(r) => format!("{} serves={}", r.sip_addr(), r.serves(call_ref)),
                None => "none yet".to_string(),
            },
            survivor.serves(call_ref),
        ),
    );
    dialog
}

/// Pump until no node holds a trace of `call_ref`, then assert the cluster-wide
/// release. Answers keepalive OPTIONS along the way.
async fn settle_released(
    fh: &FailoverHarness,
    alice: &Agent,
    bob: &Agent,
    nodes: &[&ReplicatedB2buaSut],
    call_ref: &str,
) {
    let released = fh
        .pump_until(Duration::from_secs(1), Duration::from_secs(40), async || {
            if let Some(mut t) = alice.try_receive_tolerating("OPTIONS", &[]).await {
                t.respond(200, "OK").await;
            }
            if let Some(mut t) = bob.try_receive_tolerating("OPTIONS", &[]).await {
                t.respond(200, "OK").await;
            }
            let mut clean = true;
            for n in nodes {
                if n.holds_any_trace(call_ref).await {
                    clean = false;
                }
            }
            clean
        })
        .await;
    assert!(released, "every node released {call_ref} within Timer H of the teardown");
    assert_call_fully_released(nodes, call_ref).await;
}

fn report_dir(stem: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/seq-reports").join(stem)
}

/// How the elder incarnation leaves: the three removals under test.
#[derive(Clone, Copy)]
enum Removal {
    /// Endpoint gone from the slice, SIGTERM answered by a drain, SIGKILL
    /// mid-window.
    Forced,
    /// Endpoint `terminating` but still in the slice (withdrawn from routing,
    /// still pulled), SIGTERM answered by a drain, no kill — the process exits
    /// when its grace runs out.
    Graceful,
    /// The process dies WITH its endpoint: no window at all.
    Crash,
}

/// One run of the scenario, from the ringing call to the cluster-wide release.
/// The assertions state the outcome the callee's answer is owed: it reaches the
/// caller whichever incarnation sent the INVITE, and her already-ACKed INVITE
/// server transaction carries no further final.
async fn run(name: &str, title: &str, removal: Removal) {
    let Cluster { mut fh, alice, bob, proxy, mut w_b1, mut w_b2 } = spawn_cluster(name).await;

    // ── ring: the route arms NoAnswer and the ringing version reaches the backup
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak_ord) = worker_ordinals(uas.request());
    let (elder, survivor): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    fh.advance(Duration::from_millis(500)).await;

    let call_ref = find_backed_up_ref(survivor, &pri_ord).await;
    let fire_at = ring_deadline(survivor, &pri_ord, &call_ref).await;
    assert!(elder.serves(&call_ref), "the primary serves the ringing call");

    // ── t0: the endpoint is withdrawn; the process is signalled ──────────────
    // From here the dialog has two potential owners of one leg, so ADR-0014's
    // accepted in-dialog CSeq overlap is in scope; the establishment above kept
    // the rule fully gating.
    fh.accept_rfc_deviations_from_now(
        RULE_CSEQ_IN_DIALOG_ORDER,
        "ADR-0014 accepted trade-off: dual-owner in-dialog CSeq overlap while an \
         ordinal runs two incarnations",
    );
    let t0 = fh.now_ms();
    match removal {
        Removal::Graceful => fh.withdraw_routing(&pri_ord),
        _ => fh.withdraw(&pri_ord),
    }
    // A `w_bak` cookie minted on the withdrawn member resolves to nothing until
    // the replacement joins: in a two-worker cluster the survivor's own calls
    // have no failover for the window.
    assert!(proxy.health(&pri_ord).is_none(), "the withdrawn ordinal does not resolve");
    let mut drain: Option<failover_harness::PendingDrain> = match removal {
        Removal::Crash => {
            fh.mark(&pri_ord, None, "crash", "the process dies with its endpoint");
            elder.crash();
            None
        }
        _ => Some(fh.begin_drain_pending(elder, BOUNDS)),
    };

    // ── t0 + 0.4 s (A, C): a replacement of the SAME ordinal, alongside ──────
    // In the graceful case (B) a StatefulSet creates the replacement only once
    // the elder's pod is gone, so none exists yet.
    let mut replacement: Option<ReplicatedB2buaSut> = match removal {
        Removal::Graceful => None,
        _ => {
            advance_to(&fh, t0, REPLACEMENT_AT).await;
            let replacement = fh.spawn_replacement(&pri_ord).await;
            let reclaimed = fh
                .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
                    replacement.is_ready() && replacement.serves(&call_ref)
                })
                .await;
            if let Some(d) = drain.as_mut() {
                d.poll();
            }
            assert!(reclaimed, "the replacement bootstrapped and reclaimed the ringing copy");
            fh.mark(
                &pri_ord,
                None,
                "reclaimed",
                &format!("the replacement holds the ringing copy at +{} ms", fh.now_ms() - t0),
            );
            Some(replacement)
        }
    };

    // ── t0 + 1.0 s: the callee answers, inside the elder's window ────────────
    let mut dialog = answer_inside_window(
        &mut fh,
        &pri_ord,
        t0,
        &mut uas,
        &mut call,
        drain.as_mut(),
        elder,
        replacement.as_ref(),
        survivor,
        &call_ref,
    )
    .await;

    // ── how the elder leaves, and when its replacement is published ──────────
    let survivor_key = format!("{}#g1", survivor.ordinal());
    match removal {
        Removal::Forced | Removal::Crash => {
            // t0 + 1.8 s: the replacement's address is published.
            advance_to(&fh, t0, READMIT_AT).await;
            fh.readmit(&pri_ord, replacement.as_ref().expect("spawned alongside").sip_addr());
            if let Removal::Forced = removal {
                // t0 + 2.0 s: SIGKILL ends the drain with the process.
                advance_to(&fh, t0, SIGKILL_AT).await;
                drop(drain.take());
                fh.mark(&pri_ord, None, "crash", "SIGKILL ends the drained incarnation");
                elder.crash();
            } else {
                drop(drain.take());
            }
        }
        Removal::Graceful => {
            // The survivor's supervisor pulls the terminating member on presence
            // alone (ADR-0031 D1): the link reads kept, and nothing parks it
            // while the drain runs out with the endpoint still in the slice.
            assert_eq!(
                survivor.peer_link(&pri_ord),
                failover_harness::PeerLink::Kept,
                "the survivor keeps pulling the terminating member"
            );
            let mut outcome = None;
            for _ in 0..120 {
                fh.advance(Duration::from_millis(100)).await;
                if let Some(o) = drain.as_mut().and_then(|d| d.poll()) {
                    outcome = Some(o);
                    break;
                }
            }
            drop(drain.take());
            let outcome = outcome.expect("the drain returned inside its grace");
            // D1 keeps the survivor pulling, so the elder's last flushes land and
            // its Backup flow reports the head: the drain exits on the peers
            // holding the call, not on the ceiling (ADR-0031 D2).
            assert_eq!(
                outcome.exit,
                failover_harness::DrainExit::CaughtUp,
                "the drain exited on its backup holding the call, in {:?}",
                outcome.elapsed
            );
            assert!(
                outcome.elapsed < GRACE,
                "the caught-up exit is inside the grace, got {:?}",
                outcome.elapsed
            );
            let residual = outcome.residual;
            let survivor_view = fh.view_ledger().beliefs(&survivor_key, &pri_ord);
            assert!(
                !survivor_view.contains(&Belief::PeerParked),
                "nothing parked the peer through the drain: {survivor_view:?}"
            );
            assert_eq!(
                survivor.peer_link(&pri_ord),
                failover_harness::PeerLink::Kept,
                "the link is still running when the drain returns"
            );
            assert!(
                proxy.health(&pri_ord).is_none(),
                "no replacement has joined: the ordinal still resolves to nothing"
            );
            fh.mark(
                &pri_ord,
                None,
                "exit",
                &format!("the drain returned with {residual} live call(s); the process exits"),
            );
            // The pod is gone: its endpoint leaves the slice and the process ends.
            fh.depart(&pri_ord);
            elder.crash();
            fh.advance(Duration::from_millis(200)).await;
            assert_eq!(
                survivor.peer_link(&pri_ord),
                failover_harness::PeerLink::Parked,
                "leaving the slice is what parks the peer"
            );

            // The StatefulSet creates the same ordinal again, on a new address;
            // it bootstraps from the survivor and reclaims the answered copy.
            let r = fh.spawn_replacement(&pri_ord).await;
            let reclaimed = fh
                .pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
                    r.is_ready() && r.serves(&call_ref)
                })
                .await;
            assert!(reclaimed, "the replacement bootstrapped and reclaimed the answered copy");
            fh.mark(
                &pri_ord,
                None,
                "reclaimed",
                &format!("the replacement holds the answered copy at +{} ms", fh.now_ms() - t0),
            );
            fh.readmit(&pri_ord, r.sip_addr());
            replacement = Some(r);
        }
    }
    let replacement = replacement.expect("the replacement exists in every case");
    assert_eq!(
        proxy.health(&pri_ord),
        Some(WorkerHealth::Unknown),
        "the ordinal resolves again once the replacement joins"
    );

    // ── t0 + 2.2 s: the replacement is judged alive ──────────────────────────
    advance_to(&fh, t0, ALIVE_AT).await;
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_millis(500)).await;

    // ── the ring deadline on the reclaimed copy ──────────────────────────────
    assert!(fh.now_ms() < fire_at, "the window closes before the ring deadline");
    let crossed = cross_deadline(&fh, &call, &alice, &bob, fire_at).await;
    // An answered call owes the caller nothing more and the callee no CANCEL.
    let passed = crossed.finals_to_alice.is_empty() && crossed.cancels_to_bob == 0;
    let written = fh
        .write_unified_report(&report_dir(name), "report", title, passed)
        .expect("report written");
    eprintln!("report: {}", written[0].display());

    assert_eq!(
        crossed.stray_final(),
        None,
        "a second final on the caller's already-ACKed INVITE server transaction \
         (RFC 3261 §17.2.1) — finals {:?}",
        crossed.finals_to_alice,
    );
    assert_eq!(crossed.cancels_to_bob, 0, "the callee's answered INVITE takes no CANCEL");

    // ── the call terminates end-to-end and the cluster releases it ───────────
    let nodes: [&ReplicatedB2buaSut; 3] = [&*elder, survivor, &replacement];
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    settle_released(&fh, &alice, &bob, &nodes[..], &call_ref).await;
    assert_eq!(total_cdrs_for(&nodes[..], &call_ref), 1, "exactly one CDR across the cluster");

    // The endpoint-scoped audit, stated rather than left to the Drop backstop —
    // the per-bind suite is not an oracle here, since the call legitimately
    // changes worker binds mid-dialog.
    fh.assert_sip_rfc_clean(name);

    drop((w_b1, w_b2, replacement, proxy));
}

/// **Forced removal.** The endpoint is withdrawn, the SIGTERM is answered by a
/// drain and the SIGKILL lands 2 s later. The callee answers in between: the
/// elder's address is tombstoned, so the 2xx reverse-fails to the survivor,
/// which answers the caller and takes the dialog over. The ring deadline on the
/// replacement's reclaimed copy must not author a second final.
#[tokio::test(start_paused = true)]
async fn a_force_removed_primary_answering_in_its_window_draws_no_second_final() {
    run(
        "withdrawn-primary-answers-forced",
        "A force-removed primary answers inside its window",
        Removal::Forced,
    )
    .await;
}

/// **Graceful removal.** The same withdrawal and the same drain, with no kill at
/// all: the process exits when its grace runs out — the window is the drain, not
/// the force, and the answer takes the same route through it.
#[tokio::test(start_paused = true)]
async fn a_gracefully_removed_primary_answering_in_its_drain_draws_no_second_final() {
    run(
        "withdrawn-primary-answers-graceful",
        "A gracefully removed primary answers inside its drain",
        Removal::Graceful,
    )
    .await;
}

/// **Control: a true crash.** The process dies WITH its endpoint, so there is no
/// window at all. The address it left behind is tombstoned all the same, so the
/// answer reaches the survivor rather than the socket nobody is bound to.
#[tokio::test(start_paused = true)]
async fn a_primary_that_dies_with_its_endpoint_still_has_its_answer_delivered() {
    run(
        "withdrawn-primary-answers-crashed",
        "A primary that dies with its endpoint",
        Removal::Crash,
    )
    .await;
}
