//! **The ACK of a 2xx, the first provisional of an INVITE and the final to a
//! non-INVITE request leave once the backup applied the write that records
//! what they answer, or at the cap** (ADR-0055 §3). They are the messages
//! that stop a peer retransmitting: the callee retransmits its 2xx until the
//! ACK reaches it (RFC 3261 §13.3.1.4), the caller its INVITE until a
//! provisional does (§17.1.1.2), a sender its request until its final
//! (§17.1.2.2), so a primary
//! that dies before its backup holds the write leaves the peer a
//! retransmission for the backup to take. Every other message leaves at once,
//! and a held ACK leaves before any later message on its dialog (§14.1).
//!
//! ```text
//!   replication delayed 20 ms each way   the caller's 100 trails her INVITE by
//!                                        the round trip
//!   replication delayed 40 ms            the callee's ACK trails the caller's by the delay
//!   replication stalled                  the ACK leaves at the cap, never later
//!   stalled, the callee's INFO           its relayed 200 leaves at the cap
//!   stalled, the caller's BYE            the callee reads the ACK before the BYE,
//!     right behind her ACK               and the BYE is not held
//!   the primary dies after its backup    the callee's 2xx repeat draws the
//!     applied, the ACK still held        copy's ACK (§13.2.2.4)
//! ```

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::{CallDecisionEngine, ScriptedDecisionEngine};
use b2bua::limiter::NoopLimiter;
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness,
    ReplicatedB2buaSut, WorkerHealth,
};
use scenario_harness::{Agent, Dialog};
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

/// The hold's default cap: half the §17.2.1 bound of the first provisional.
const CAP: Duration = Duration::from_millis(100);
/// The step the parties read their sockets at.
const STEP: Duration = Duration::from_millis(1);

/// Which way the replication from the primary to its backup is slowed once
/// the callee answered.
#[derive(Clone, Copy)]
enum Replication {
    Delayed(u64),
    Stalled,
}

struct Cluster {
    fh: FailoverHarness,
    alice: Agent,
    bob: Agent,
    _proxy: failover_harness::ProxySut,
    w_b1: ReplicatedB2buaSut,
    w_b2: ReplicatedB2buaSut,
}

async fn cluster(name: &str) -> Cluster {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let decision = || -> Arc<dyn CallDecisionEngine> {
        Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070))
    };
    let (rt, ob) = (("127.0.0.1", 5070), ("127.0.0.1", 5080));
    let noop = || Arc::new(NoopLimiter);
    let w_b1 = fh.spawn_worker_limited("b1", "b1", B1, &["b2"], rt, ob, decision(), noop()).await;
    let w_b2 = fh.spawn_worker_limited("b2", "b2", B2, &["b1"], rt, ob, decision(), noop()).await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");
    Cluster { fh, alice, bob, _proxy: proxy, w_b1, w_b2 }
}

/// Advance in [`STEP`]s until `bob` reads a request, `bound` at most; the
/// request's method and how long it took.
async fn bob_reads(c: &Cluster, bound: Duration) -> Option<(Method, Duration)> {
    let start = tokio::time::Instant::now();
    loop {
        if let Some(msg) = c.bob.take_queued().await {
            match msg {
                SipMessage::Request(r) => return Some((r.method().clone(), start.elapsed())),
                other => panic!("unexpected toward bob: {other:?}"),
            }
        }
        if start.elapsed() >= bound {
            return None;
        }
        sip_clock::testkit::advance_settled(STEP).await;
    }
}

/// alice calls bob through the proxy; bob answers at once. The replication
/// from the primary to its backup is slowed as `slow` says once bob's 200
/// reached alice, before her ACK. Returns alice's dialog, bob's ACK delay,
/// the primary's ordinal and the call's ref.
async fn answered(c: &mut Cluster, slow: Replication) -> (Dialog, Duration, String, String) {
    let mut call = c.alice.invite(&c.bob).with_sdp(OFFER).through(c._proxy.addr()).send().await;
    let mut uas = c.bob.receive("INVITE").await;
    let (primary, backup) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    c.fh.advance(Duration::from_millis(300)).await;
    match slow {
        Replication::Delayed(ms) => c.fh.delay(&primary, &backup, ms),
        Replication::Stalled => c.fh.stall(&primary, &backup),
    }
    let dialog = call.ack().await;
    let (method, after) = bob_reads(c, Duration::from_secs(1)).await.expect("bob reads the ACK");
    assert_eq!(method, Method::Ack, "the first request bob reads is the ACK");
    let primary_node = if primary == "b1" { &c.w_b1 } else { &c.w_b2 };
    let call_ref = primary_node.scan_primary(&primary).pop().expect("the primary holds the call");
    (dialog, after, primary, call_ref)
}

/// bob answers alice's BYE; the call settles to one CDR on a clean cluster.
async fn ends(c: Cluster, mut dialog: Dialog, call_ref: &str) {
    scenario_harness::callflow::hangup(&mut dialog, &c.bob).await;
    let drained =
        c.fh.settle_terminal(async || c.w_b1.memory_clean() && c.w_b2.memory_clean()).await;
    assert!(drained, "both nodes release the call");
    c.fh.linger_peers(&[&c.alice, &c.bob], Duration::from_secs(3)).await;
    assert_eq!(total_cdrs_for(&[&c.w_b1, &c.w_b2], call_ref), 1, "exactly one CDR");
    assert_call_fully_released(&[&c.w_b1, &c.w_b2], call_ref).await;
}

#[tokio::test(start_paused = true)]
async fn the_first_100_trails_the_replication_of_the_new_call() {
    let mut c = cluster("held-trying-delayed").await;
    c.fh.delay("b1", "b2", 20);
    c.fh.delay("b2", "b1", 20);
    let sent = tokio::time::Instant::now();
    let mut call = c.alice.invite(&c.bob).with_sdp(OFFER).through(c._proxy.addr()).send().await;
    call.expect(100).await;
    let after = sent.elapsed();
    assert!(
        after >= Duration::from_millis(40),
        "the 100 left before the backup applied: {after:?}"
    );
    assert!(after < CAP, "the 100 left once the backup applied, not at the cap: {after:?}");
    c.fh.heal("b1", "b2");
    let mut uas = c.bob.receive("INVITE").await;
    let (primary, _) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    c.bob.receive("ACK").await;
    let primary_node = if primary == "b1" { &c.w_b1 } else { &c.w_b2 };
    let call_ref = primary_node.scan_primary(&primary).pop().expect("the primary holds the call");
    ends(c, dialog, &call_ref).await;
}

#[tokio::test(start_paused = true)]
async fn the_ack_trails_the_replication_of_the_answer() {
    let mut c = cluster("held-ack-delayed").await;
    let (dialog, after, primary, call_ref) = answered(&mut c, Replication::Delayed(40)).await;
    assert!(
        after >= Duration::from_millis(40),
        "the ACK left before the backup applied the answer: {after:?} after alice's"
    );
    assert!(after < CAP, "the ACK left once the backup applied it, not at the cap: {after:?}");
    let backup = if primary == "b1" { "b2" } else { "b1" };
    c.fh.heal(&primary, backup);
    ends(c, dialog, &call_ref).await;
}

#[tokio::test(start_paused = true)]
async fn a_stalled_backup_holds_the_ack_no_longer_than_the_cap() {
    let mut c = cluster("held-ack-stalled").await;
    let (dialog, after, primary, call_ref) = answered(&mut c, Replication::Stalled).await;
    assert!(after >= CAP, "the ACK left before the cap on a stalled backup: {after:?}");
    assert!(after <= CAP + Duration::from_millis(5), "the ACK outlived the cap: {after:?}");
    let backup = if primary == "b1" { "b2" } else { "b1" };
    c.fh.resume(&primary, backup);
    ends(c, dialog, &call_ref).await;
}

#[tokio::test(start_paused = true)]
async fn a_stalled_backup_holds_the_final_to_a_request_no_longer_than_the_cap() {
    let mut c = cluster("held-final-stalled").await;
    let mut call = c.alice.invite(&c.bob).with_sdp(OFFER).through(c._proxy.addr()).send().await;
    let mut uas = c.bob.receive("INVITE").await;
    let (primary, backup) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    let mut bob_dialog = uas.dialog();
    call.expect(200).await;
    let dialog = call.ack().await;
    c.bob.receive("ACK").await;
    c.fh.advance(Duration::from_millis(300)).await;
    c.fh.stall(&primary, &backup);

    let mut info = bob_dialog.request(sip_message::generators::InDialogMethod::Info, None).await;
    let mut relayed = c.alice.receive("INFO").await;
    let answered = tokio::time::Instant::now();
    relayed.respond(200, "OK").await;
    info.expect(200).await;
    let after = answered.elapsed();
    assert!(after >= CAP, "the 200 left before the cap on a stalled backup: {after:?}");
    assert!(after <= CAP + Duration::from_millis(5), "the 200 outlived the cap: {after:?}");

    c.fh.resume(&primary, &backup);
    let primary_node = if primary == "b1" { &c.w_b1 } else { &c.w_b2 };
    let call_ref = primary_node.scan_primary(&primary).pop().expect("the primary holds the call");
    ends(c, dialog, &call_ref).await;
}

#[tokio::test(start_paused = true)]
async fn a_held_ack_leaves_before_the_bye_behind_it() {
    let mut c = cluster("held-ack-then-bye").await;
    let mut call = c.alice.invite(&c.bob).with_sdp(OFFER).through(c._proxy.addr()).send().await;
    let mut uas = c.bob.receive("INVITE").await;
    let (primary, backup) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    c.fh.advance(Duration::from_millis(300)).await;
    c.fh.stall(&primary, &backup);
    let mut dialog = call.ack().await;
    let sent = tokio::time::Instant::now();
    let mut bye = dialog.bye().await;
    c.bob.receive("ACK").await;
    assert!(sent.elapsed() < CAP, "the BYE released the held ACK: {:?}", sent.elapsed());
    c.bob.receive("BYE").await.respond(200, "OK").await;
    assert!(sent.elapsed() < CAP, "the BYE behind a held ACK is not held: {:?}", sent.elapsed());
    bye.expect(200).await;
    c.fh.resume(&primary, &backup);
    let primary_node = if primary == "b1" { &c.w_b1 } else { &c.w_b2 };
    let drained =
        c.fh.settle_terminal(async || c.w_b1.memory_clean() && c.w_b2.memory_clean()).await;
    assert!(drained, "both nodes release the call");
    c.fh.linger_peers(&[&c.alice, &c.bob], Duration::from_secs(3)).await;
    let cdrs: Vec<_> = primary_node.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR");
    assert_call_fully_released(&[&c.w_b1, &c.w_b2], &cdrs[0].call_ref).await;
}

/// The last 2xx to an INVITE `from` put on the wire, and where it went.
fn last_invite_2xx(
    fh: &FailoverHarness,
    from: std::net::SocketAddr,
) -> (Vec<u8>, std::net::SocketAddr) {
    let parser = CustomParser::new();
    fh.sip_entries()
        .into_iter()
        .rev()
        .find(|e| {
            e.from == from
                && matches!(parser.parse(&e.raw), Ok(SipMessage::Response(r))
                    if r.status() == 200 && *r.cseq().method() == Method::Invite)
        })
        .map(|e| (e.raw, e.to))
        .expect("the callee answered")
}

#[tokio::test(start_paused = true)]
async fn an_ack_the_kill_takes_is_resent_by_the_copy_to_the_2xx_repeat() {
    let mut c = cluster("held-ack-killed").await;
    let mut call = c.alice.invite(&c.bob).with_sdp(OFFER).through(c._proxy.addr()).send().await;
    let mut uas = c.bob.receive("INVITE").await;
    let (primary, backup) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    c.fh.advance(Duration::from_millis(300)).await;
    c.fh.delay(&primary, &backup, 20);
    c.fh.delay(&backup, &primary, 20);
    let primary_node = if primary == "b1" { &c.w_b1 } else { &c.w_b2 };
    let call_ref = primary_node.scan_primary(&primary).pop().expect("the primary holds the call");

    // ── the backup applies the ACK's write; the primary dies before it learns ─
    let dialog = call.ack().await;
    assert!(bob_reads(&c, Duration::from_millis(30)).await.is_none(), "the ACK is still held");
    c.fh.mark(&primary, None, "crash", "the ACK held, its write applied at the backup");
    if primary == "b1" {
        c.w_b1.crash()
    } else {
        c.w_b2.crash()
    };
    c._proxy.set_health(&primary, WorkerHealth::Dead);
    c.fh.heal(&backup, &primary);

    // ── the callee repeats its 2xx; the copy ACKs it ─────────────────────────
    let (repeat, dst) = last_invite_2xx(&c.fh, c.bob.addr());
    c.bob.try_send_datagram(&repeat, dst).await.expect("the 2xx repeat leaves");
    let (method, _) = bob_reads(&c, Duration::from_millis(200)).await.expect("bob reads an ACK");
    assert_eq!(method, Method::Ack, "the copy ACKs the 2xx repeat");

    // ── the primary's replacement reclaims the call and discharges its CDR ───
    let replacement = c.fh.spawn_replacement(&primary).await;
    assert!(c.fh.readmit_once_ready(&replacement).await, "the replacement became ready");
    let reclaimed =
        c.fh.pump_until(Duration::from_millis(100), Duration::from_secs(30), async || {
            replacement.serves(&call_ref)
        })
        .await;
    assert!(reclaimed, "the replacement reclaimed the call");
    c._proxy.set_health(&primary, WorkerHealth::Alive);
    if primary == "b1" {
        c.w_b1 = replacement
    } else {
        c.w_b2 = replacement
    };
    c.fh.advance(Duration::from_millis(500)).await;
    ends(c, dialog, &call_ref).await;
}
