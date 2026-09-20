//! **A forward flush never regresses the backup's progress** (ADR-0031 D3).
//!
//! A primary vanishes behind a network partition: unreachable on both planes,
//! its process alive and its own timers still turning its calls. The proxy's
//! probe reads it `Dead`, so the callee's 2xx reverse-fails to the cookie's
//! backup, whose takeover copy answers the caller and bumps `b` on the
//! `bak:{primary}` Element. Behind the cut the primary still reads the call as
//! ringing, reaches its route-time ring deadline, and tears its own copy down —
//! bumping `p`, never `b`.
//!
//! ```text
//!   ringing · primary serves · survivor holds the ringing Element (p,0)
//!   t0        the primary vanishes: repl fabric cut, SIP plane cut, probe Dead
//!   +0.3 s    the callee's 200 reverse-fails to the survivor
//!             the survivor answers the caller; the Element carries (p, 1)
//!   +30 s     behind the cut the primary's ring deadline fires: 480 + CANCEL
//!             leave nowhere, its copy goes Terminating and then Terminated
//!   +33 s     the survivor's takeover copy self-releases at Timer H — the
//!             Element is all that is left of the answered call
//!   heal      the primary's pending forward flush lands on the survivor
//!             Put cell:    a `Put (p+1, 0)` carrying its teardown
//!             Delete cell: the `Delete` its discharge propagated
//!   after     an in-dialog request re-takes the call over from the Element
//!   BYE       the SIP plane heals; the primary's CANCEL ladder, which outlives
//!             the copy it tore down (ADR-0034), reaches the callee: 481 (§9.2)
//! ```
//!
//! Neither may land: the primary's teardown is a branch off a view the survivor
//! left behind, not a newer version of it. If either does, the answered call is
//! destroyed on the only node that still holds it — the caller's next in-dialog
//! request finds no call, or re-materialises a torn-down one and authors a second
//! final on an INVITE server transaction she already ACKed (RFC 3261 §17.2.1,
//! §13.3.1.4).
//!
//! Each cell runs twice, on [`HealAt`]: with the cut healing while the survivor
//! still SERVES its takeover copy, and with it healing once that copy has
//! self-released and the Element is the call's last record.
//!
//! The heal here delivers the whole history the cut held back, in order — a real
//! reconnect compacts the peer log to one entry per ref first, so the Delete
//! cell's exposure is narrower in production than it is here: the survivor sees
//! every intermediate version of the primary's teardown, not just its last.
//!
//! Residual, measured rather than narrated (ADR-0031 D3): the partitioned primary
//! tore down a call the survivor kept serving. The Put cell ends with the answered
//! call's one record, discharged by the primary once it reclaims the survivor's
//! terminal. The Delete cell ends with the primary's own no-answer record and NO
//! record of the answered call: the primary discharged behind the cut, and its
//! resurrection tombstone then refuses the terminal the survivor defers to it, so
//! that terminal is counted lost when the Element ages out.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallTreatment, NewCallResponse, RejectDecision, ScriptedDecisionEngine,
};
use b2bua::limiter::NoopLimiter;
use call::{CallBodyCodec, CallModelState, CdrEventType, LegState, MsgpackCodec, TimerType};
use failover_harness::{
    worker_ordinals, FailoverHarness, PartitionRole, ProxySut, ReplicatedB2buaSut, WorkerHealth,
    RULE_CSEQ_IN_DIALOG_ORDER, RULE_NO_CANCEL_AFTER_FINAL,
};
use repl_net::frame::{Frame, Op};
use repl_net::transport::Direction;
use scenario_harness::Agent;
use sip_message::generators::InDialogMethod;
use sip_message::SipMessage;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

const BAK: PartitionRole = PartitionRole::Backup;

/// Past the replica TTL (`reboot_budget`, 600 s) plus the reap's 30 s cadence:
/// how long a deferred terminal no primary comes back for takes to leave.
const REPLICA_TTL_WAIT: Duration = Duration::from_secs(700);

/// The deployed ring deadline: armed at route time on both copies of the call.
const NO_ANSWER_SEC: i64 = 30;

/// Why `cseq-in-dialog-order` is accepted from the partition onward: two
/// potential owners of one leg mint b-leg CSeqs independently (ADR-0014).
const CSEQ_OVERLAP: &str =
    "ADR-0014 accepted trade-off: dual-owner in-dialog CSeq overlap while a partitioned \
     primary and its backup both hold the call";

/// Why `no-cancel-after-final` is accepted from the SIP heal onward: the
/// partitioned primary's ring-deadline CANCEL outlives the copy it tore down
/// (ADR-0034) and crosses once the plane heals, on the proxy lane that relayed
/// the other owner's ACK — the dual-owner residual of ADR-0014, answered 481.
const STALE_CANCEL: &str =
    "ADR-0014 accepted trade-off: a partitioned owner's CANCEL ladder (ADR-0034) crosses the \
     healed SIP plane after its backup answered the same leg";

/// The deployed shape: a failover-capable route (callback context) arming the
/// per-b-leg `NoAnswer`, whose `no_answer_timeout` failure consult answers with
/// a `480` reject.
fn no_answer_decision() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.no_answer_timeout_sec = Some(NO_ANSWER_SEC);
                r.callback_context = Some("forward-flush-guard-ctx".into());
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

/// The `bak:{primary}` Element as `(call state, a-leg state)`, or `None` when
/// the node holds no Element for the ref at all.
async fn element_state(
    backup: &ReplicatedB2buaSut,
    primary: &str,
    call_ref: &str,
) -> Option<(CallModelState, LegState)> {
    let body = backup.get(BAK, primary, call_ref).await?;
    let call: call::Call = MsgpackCodec::new().decode(&body).expect("Element body decodes");
    Some((call.state, call.a_leg.state))
}

/// The absolute deadline of the replica's per-b-leg `NoAnswer` entry.
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

/// The cluster-side destinations [`hops`] recorded, in wire order.
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

/// What crossing a window did on the wire: every INVITE final the caller took,
/// and the CANCELs the callee's answered INVITE drew.
struct Crossing {
    finals_to_alice: Vec<u16>,
    cancels_to_bob: usize,
}

/// Pump `secs` seconds of virtual time, collecting what reaches the peers. The
/// callee's INVITE transaction took its 2xx, so a CANCEL reaching it draws a 481
/// (RFC 3261 §9.2); every non-2xx final the caller takes is hop-ACKed, as a real
/// UAC's transaction layer does (§17.1.1.3).
async fn sweep(
    fh: &FailoverHarness,
    call: &scenario_harness::ClientInvite,
    alice: &Agent,
    bob: &Agent,
    secs: u64,
) -> Crossing {
    let mut out = Crossing { finals_to_alice: Vec::new(), cancels_to_bob: 0 };
    for _ in 0..secs {
        fh.advance(Duration::from_secs(1)).await;
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

/// Answer what the peers take once the SIP plane is healed: a keepalive OPTIONS
/// with 200, and a CANCEL with 481 — the callee's INVITE transaction took its
/// 2xx long ago (RFC 3261 §9.2). The CANCEL is the partitioned primary's: the
/// ring deadline's CANCEL client transaction outlives the copy it tore down and
/// keeps its Timer E ladder to Timer F (ADR-0034), so a retransmission crosses
/// as soon as the cut lifts. Returns how many CANCELs the callee answered.
async fn answer_healed_peers(alice: &Agent, bob: &Agent) -> usize {
    if let Some(mut t) = alice.try_receive_tolerating("OPTIONS", &[]).await {
        t.respond(200, "OK").await;
    }
    let mut cancels = 0;
    while let Some(mut cancel) = bob.try_receive_tolerating("CANCEL", &["OPTIONS"]).await {
        cancels += 1;
        cancel.respond(481, "Call/Transaction Does Not Exist").await;
    }
    cancels
}

/// Pump until no node holds a trace of `call_ref`, then assert the cluster-wide
/// release. Answers the healed peers along the way and returns the CANCELs the
/// callee took.
async fn settle_released(
    fh: &FailoverHarness,
    alice: &Agent,
    bob: &Agent,
    nodes: &[&ReplicatedB2buaSut],
    call_ref: &str,
) -> usize {
    let mut cancels = 0;
    let released = fh
        .pump_until(Duration::from_secs(1), Duration::from_secs(90), async || {
            cancels += answer_healed_peers(alice, bob).await;
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
    failover_harness::assert_call_fully_released(nodes, call_ref).await;
    cancels
}

/// The CDRs `nodes` wrote for `call_ref` that record an `Answer` event — the
/// answered call's own record, told apart from the stale no-answer record the
/// partitioned primary writes for the copy it tore down (the D3 residual).
fn answered_cdrs(nodes: &[&ReplicatedB2buaSut], call_ref: &str) -> usize {
    nodes
        .iter()
        .map(|n| {
            n.cdr_records()
                .into_iter()
                .filter(|r| {
                    r.call_ref == call_ref
                        && r.events.iter().any(|e| e.event_type == CdrEventType::Answer)
                })
                .count()
        })
        .sum()
}

fn report_dir(stem: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/seq-reports").join(stem)
}

/// What the partitioned primary flushes forward once the heal lets it through.
#[derive(Clone, Copy)]
enum Pending {
    /// Its teardown-in-progress body, at `(p+1, 0)` — a `Put` behind the
    /// Element's `b`.
    Put,
    /// The delete its discharge propagated once the teardown completed.
    Delete,
}

impl Pending {
    /// The residual this cell ends on, stated on the timeline rather than
    /// asserted away (ADR-0031 D3).
    fn residual(self) -> &'static str {
        match self {
            Pending::Put => {
                "the partitioned primary tore its own unanswered copy down; it reclaims the \
                 survivor's terminal and writes the answered call's one record"
            }
            Pending::Delete => {
                "the partitioned primary discharged its own unanswered copy behind the cut, so \
                 its no-answer record is the call's only one: the terminal the survivor defers \
                 to it is reclaimed by nobody and ages out at the replica TTL"
            }
        }
    }
}

/// The caller's in-dialog re-INVITE, answered end to end. Whichever node the
/// proxy routes it to re-materialises the call from the Element to serve it.
async fn reinvite(
    fh: &FailoverHarness,
    dialog: &mut scenario_harness::Dialog,
    bob: &Agent,
    offer: &str,
) {
    let mut round = dialog.request(InDialogMethod::Invite, Some(offer)).await;
    let mut peer = bob.receive_tolerating("INVITE", &["OPTIONS"]).await;
    peer.respond(200, "OK").with_sdp(ANSWER).await;
    round.expect_tolerating(200, &["OPTIONS"]).await;
    dialog.ack(None).await;
    bob.receive_tolerating("ACK", &["OPTIONS"]).await;
    fh.advance(Duration::from_millis(500)).await;
}

/// When the cut heals — the two moments the refusal has to hold at.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HealAt {
    /// While the survivor still SERVES the takeover copy: the refusal must leave
    /// the live call alone. Nothing is folded (the flush carries no progress the
    /// copy lacks), so no CDR is written, no delete is propagated back, and the
    /// call plays on to its own BYE.
    BeforeTimerH,
    /// Once that copy has self-released and the Element is the call's last
    /// record: the flush must not regress or evict it.
    AfterShed,
}

/// The CDRs `nodes` wrote for `call_ref`.
fn cdrs_on(node: &ReplicatedB2buaSut, call_ref: &str) -> usize {
    node.cdr_records().into_iter().filter(|r| r.call_ref == call_ref).count()
}

/// Every replication `Delete` for `call_ref` that left `ordinal`'s listener —
/// what an acting backup discharging a folded body would propagate back to the
/// primary. The lane map names each node's listen address, and a listener's
/// frames are the ones it SENDS from that address.
fn deletes_sent_by(fh: &FailoverHarness, ordinal: &str, call_ref: &str) -> usize {
    let report = fh.repl_report();
    let listener = report
        .lanes
        .iter()
        .find(|(_, ord)| ord.as_str() == ordinal)
        .map(|(addr, _)| *addr)
        .expect("the node's replication lane is named");
    report
        .frames
        .iter()
        .filter(|f| f.dir == Direction::Sent && f.from == listener)
        .filter(|f| {
            matches!(&f.frame, Frame::Data { op: Op::Delete, call_ref: r, .. } if r == call_ref)
        })
        .count()
}

/// One run of the scenario, from the ringing call to the cluster-wide release.
async fn run(name: &str, title: &str, pending: Pending, heal_at: HealAt) {
    let Cluster { mut fh, alice, bob, proxy, mut w_b1, mut w_b2 } = spawn_cluster(name).await;

    // ── ring: the route arms NoAnswer and the ringing version reaches the backup
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak_ord) = worker_ordinals(uas.request());
    let (primary, survivor): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
        if pri_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
    let bak_ord = survivor.ordinal().to_string();
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    fh.advance(Duration::from_millis(500)).await;

    let call_ref = find_backed_up_ref(survivor, &pri_ord).await;
    let fire_at = ring_deadline(survivor, &pri_ord, &call_ref).await;
    assert!(primary.serves(&call_ref), "the primary serves the ringing call");

    // ── the primary vanishes: both planes cut, the probe reads it Dead ───────
    fh.accept_rfc_deviations_from_now(RULE_CSEQ_IN_DIALOG_ORDER, CSEQ_OVERLAP);
    let pri_sip = primary.sip_addr();
    fh.partition(&pri_ord, &bak_ord);
    fh.cut_signalling(&pri_ord, pri_sip);
    proxy.set_health(&pri_ord, WorkerHealth::Dead);
    fh.advance(Duration::from_millis(300)).await;

    // ── the callee answers: the 2xx reverse-fails to the survivor ────────────
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive_tolerating("ACK", &["OPTIONS"]).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(survivor.serves(&call_ref), "the survivor's takeover copy serves the answered call");
    assert_eq!(
        element_state(survivor, &pri_ord, &call_ref).await,
        Some((CallModelState::Active, LegState::Confirmed)),
        "the Element carries the answer the survivor gave the caller",
    );
    let answered_b = survivor.call_bgen(BAK, &pri_ord, &call_ref).expect("the Element has a (p,b)");
    assert!(answered_b > 0, "the acting backup authored the Element: b = {answered_b}");
    fh.mark(
        &bak_ord,
        Some(&pri_ord),
        "takeover",
        &format!(
            "the survivor answered the caller; Element (p,b) = ({:?},{answered_b}); 2xx relayed \
             to {:?}",
            survivor.call_gen(BAK, &pri_ord, &call_ref),
            relayed_to(&hops(&fh, "SIP/2.0 200", "INVITE")),
        ),
    );

    // ── behind the cut the primary reaches its own ring deadline ─────────────
    let torn_down = fh
        .pump_until(Duration::from_secs(1), Duration::from_secs(60), async || {
            primary.live_call(&call_ref).is_none_or(|c| c.state != CallModelState::Active)
        })
        .await;
    assert!(torn_down, "the partitioned primary reached its ring deadline");
    assert!(fh.now_ms() >= fire_at, "the teardown is the ring deadline's, at {fire_at} ms");
    fh.mark(
        &pri_ord,
        None,
        "ring deadline",
        &format!(
            "the primary tore its unanswered copy down at +{} ms; its 480 went {:?}",
            fh.now_ms(),
            relayed_to(&hops(&fh, "SIP/2.0 480", "INVITE")),
        ),
    );

    // The Delete cell waits for the discharge that propagates the delete.
    if let Pending::Delete = pending {
        let discharged = fh
            .pump_until(Duration::from_secs(1), Duration::from_secs(90), async || {
                !primary.holds_any_trace(&call_ref).await
            })
            .await;
        assert!(discharged, "the partitioned primary discharged its own torn-down copy");
        fh.mark(&pri_ord, None, "discharge", "the delete is pending on the healed flow");
    }

    let op = match pending {
        Pending::Put => "put",
        Pending::Delete => "delete",
    };

    // ── heal while the takeover copy is still LIVE: the refusal is silent ─────
    if heal_at == HealAt::BeforeTimerH {
        // The Delete cell's primary needs its whole teardown to complete before
        // it deletes, which outlives Timer H: the caller's next in-dialog request
        // re-takes the call over, so the flush still lands under a LIVE copy.
        if !survivor.serves(&call_ref) {
            reinvite(&fh, &mut dialog, &bob, OFFER).await;
            assert!(survivor.serves(&call_ref), "the survivor re-took the call over");
        }
        let before = survivor.live_call(&call_ref).expect("the survivor serves the call");
        fh.heal(&pri_ord, &bak_ord);
        fh.advance(Duration::from_secs(1)).await;
        let after = survivor.live_call(&call_ref).expect("the live call survived the flush");
        assert_eq!(after.state, CallModelState::Active, "the live copy is untouched");
        assert_eq!(after.a_leg.state, LegState::Confirmed, "and still reads the caller answered");
        assert_eq!(
            after.timers.iter().map(|t| (t.timer_type.clone(), t.fire_at)).collect::<Vec<_>>(),
            before.timers.iter().map(|t| (t.timer_type.clone(), t.fire_at)).collect::<Vec<_>>(),
            "a refused flush carries no progress: the ledger is not resynced",
        );
        assert!(
            survivor.metrics().repl_forward_flush_refused(op) >= 1,
            "the refusal is counted, by the operation refused",
        );
        assert_eq!(
            cdrs_on(survivor, &call_ref),
            0,
            "the acting backup wrote no record: only its primary discharges (ADR-0020 X3)",
        );
        assert_eq!(
            deletes_sent_by(&fh, &bak_ord, &call_ref),
            0,
            "an acting backup propagates no delete: only its primary ends the call \
             (ADR-0014 §2 / ADR-0020 X3)",
        );
        fh.mark(&bak_ord, Some(&pri_ord), "refused (live)", "the takeover copy serves on");
    }

    // ── the takeover copy self-releases: the Element is all that is left ─────
    let shed = fh
        .pump_until(Duration::from_secs(1), Duration::from_secs(60), async || {
            !survivor.serves(&call_ref)
        })
        .await;
    assert!(shed, "the survivor's takeover copy self-released at Timer H");
    assert_eq!(
        element_state(survivor, &pri_ord, &call_ref).await,
        Some((CallModelState::Active, LegState::Confirmed)),
        "the shed copy left the answered Element behind",
    );

    // ── heal: the primary's pending forward flush lands ──────────────────────
    if heal_at == HealAt::AfterShed {
        fh.heal(&pri_ord, &bak_ord);
        fh.advance(Duration::from_secs(5)).await;
    }

    // THE HEADLINE: neither the Put nor the Delete may regress the Element.
    assert_eq!(
        element_state(survivor, &pri_ord, &call_ref).await,
        Some((CallModelState::Active, LegState::Confirmed)),
        "a forward flush regressed the answered Element the survivor is the last holder of",
    );
    assert!(
        survivor.call_bgen(BAK, &pri_ord, &call_ref).is_some_and(|b| b >= answered_b),
        "the Element never goes back below the backup counter it was authored at",
    );
    // Not `== 1`: across the heal the two owners exchange several flushes, each
    // adopting the counter the other published, and every one of the primary's
    // is refused while its view stays a branch of the call. The exact count per
    // input is pinned in `b2bua`'s `repl::s12_tests`, where one frame is one
    // frame; here what matters is that the refusal happened at all.
    assert!(
        survivor.metrics().repl_forward_flush_refused(op) >= 1,
        "the refusal is counted, by the operation refused",
    );

    // ── the caller's next in-dialog request re-takes the call over ───────────
    reinvite(&fh, &mut dialog, &bob, OFFER).await;
    assert!(survivor.serves(&call_ref), "the survivor re-materialised the call from the Element");

    // ── nothing the re-materialised copy carries ends the call by itself ─────
    let crossed = sweep(&fh, &call, &alice, &bob, 5).await;
    let passed = crossed.finals_to_alice.is_empty() && crossed.cancels_to_bob == 0;
    let written = fh
        .write_unified_report(&report_dir(name), "report", title, passed)
        .expect("report written");
    eprintln!("report: {}", written[0].display());
    assert_eq!(
        crossed.finals_to_alice,
        Vec::<u16>::new(),
        "a second final on the caller's already-ACKed INVITE server transaction (RFC 3261 §17.2.1)",
    );
    assert_eq!(crossed.cancels_to_bob, 0, "the callee's answered INVITE takes no CANCEL");

    // ── the call terminates end-to-end and the cluster releases it ───────────
    let mut bye = dialog.bye().await;
    bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    bye.expect_tolerating(200, &["OPTIONS"]).await;
    fh.accept_rfc_deviations_from_now(RULE_NO_CANCEL_AFTER_FINAL, STALE_CANCEL);
    fh.restore_signalling(&pri_ord, pri_sip);
    let nodes: [&ReplicatedB2buaSut; 2] = [&*primary, survivor];
    let lost_before = survivor.metrics().repl_terminal_lost_total();
    match pending {
        // The primary still held its own torn-down copy at the heal, so it
        // reclaims the terminal the survivor defers to it and discharges the
        // call within Timer H of the teardown. Its CANCEL ladder outlives that
        // discharge (ADR-0034); how many steps cross the healed plane is where
        // the discharge falls on it, and each draws a 481 and nothing else.
        Pending::Put => {
            let stale_cancels = settle_released(&fh, &alice, &bob, &nodes[..], &call_ref).await;
            fh.mark(
                &pri_ord,
                None,
                "stale CANCEL",
                &format!("{stale_cancels} retransmission(s) reached the callee, each answered 481"),
            );
        }
        // Nobody reclaims it: this primary discharged the call behind the cut,
        // and its resurrection tombstone refuses the terminal the survivor
        // defers to it. That Element leaves at the replica TTL instead — one
        // second at a time, answering the keepalive OPTIONS the workers send
        // along the way (a single leap would cross two deadlines at once).
        Pending::Delete => {
            // Wait on the LOSS COUNTER, not on the body: reading the body is
            // what evicts it, so a probe loop over the store would race the reap
            // and destroy the very measurement. One second at a time, answering
            // the healed peers along the way.
            let aged_out = fh
                .pump_until(Duration::from_secs(1), REPLICA_TTL_WAIT, async || {
                    answer_healed_peers(&alice, &bob).await;
                    survivor.metrics().repl_terminal_lost_total() > lost_before
                })
                .await;
            assert!(aged_out, "the unreclaimed deferred terminal left at the replica TTL");
            failover_harness::assert_call_fully_released(&nodes[..], &call_ref).await;
        }
    }
    fh.mark(&pri_ord, None, "residual", pending.residual());
    match pending {
        // One record, and it is the answered call's: the primary reclaimed the
        // terminal the survivor deferred to it.
        Pending::Put => {
            assert_eq!(
                failover_harness::total_cdrs_for(&nodes[..], &call_ref),
                1,
                "one record for the call across the cluster",
            );
            assert_eq!(answered_cdrs(&nodes[..], &call_ref), 1, "and it is the answered call's",);
        }
        // One record too — but the WRONG one. The residual is measured, not
        // narrated: the answered call has no record at all, and the terminal the
        // survivor deferred is counted as lost when the Element ages out.
        Pending::Delete => {
            assert_eq!(
                failover_harness::total_cdrs_for(&nodes[..], &call_ref),
                1,
                "one record for the call across the cluster: the primary's own",
            );
            assert_eq!(answered_cdrs(&nodes[..], &call_ref), 0, "{}", pending.residual(),);
            assert_eq!(
                survivor.metrics().repl_terminal_lost_total() - lost_before,
                1,
                "the deferred terminal nobody reclaimed is counted lost, exactly once",
            );
        }
    }

    fh.assert_sip_rfc_clean(name);
    drop((w_b1, w_b2, proxy));
}

/// **The refused `Put`.** The heal lets the primary's teardown-in-progress body
/// through at `(p+1, 0)`; the Element the survivor answered on must survive it.
#[tokio::test(start_paused = true)]
async fn a_forward_put_behind_the_elements_backup_counter_leaves_the_answered_call_alive() {
    run(
        "forward-flush-guard-put",
        "A forward Put behind the Element's backup counter",
        Pending::Put,
        HealAt::AfterShed,
    )
    .await;
}

/// **The refused `Delete`.** The primary completes its teardown behind the cut
/// and propagates the delete; the Element the survivor answered on must survive
/// it too — delete-wins yields to an authority that tore down a branch.
#[tokio::test(start_paused = true)]
async fn a_forward_delete_of_the_answered_element_leaves_the_answered_call_alive() {
    run(
        "forward-flush-guard-delete",
        "A forward Delete of an Element its backup answered on",
        Pending::Delete,
        HealAt::AfterShed,
    )
    .await;
}

/// **The refused `Put`, with the copy still live.** The same flush, landing
/// while the survivor still serves the call: it carries no progress the live
/// copy lacks, so nothing folds — the answered call is untouched and plays on.
#[tokio::test(start_paused = true)]
async fn a_forward_put_refused_under_a_live_takeover_copy_leaves_the_call_untouched() {
    run(
        "forward-flush-guard-put-live",
        "A forward Put refused under a live takeover copy",
        Pending::Put,
        HealAt::BeforeTimerH,
    )
    .await;
}

/// **The refused `Delete`, with the copy still live.** The Element is kept, the
/// copy serves on and self-releases on its own terms, and the call is still
/// there for the caller's next in-dialog request.
#[tokio::test(start_paused = true)]
async fn a_forward_delete_refused_under_a_live_takeover_copy_keeps_the_element() {
    run(
        "forward-flush-guard-delete-live",
        "A forward Delete refused under a live takeover copy",
        Pending::Delete,
        HealAt::BeforeTimerH,
    )
    .await;
}
