//! **A request in flight when the node serving the call dies** — the baseline
//! the takeover-materialise refactor must keep (the superproject's
//! `docs/todos/takeover-materialise`; `takeover-materialise/issues/NN` below is
//! that effort's `issues/` directory, spec D7/D17).
//!
//! One cell per method, all on one premise — the caller's exchange is
//! **unanswered** when the node dies — reached two ways:
//!
//! ```text
//!   established (or ringing) · primary serves · backup synchronized
//!
//!   A  alice ──REQ──▶ proxy ──▶ primary ──REQ──▶ bob    (relayed, bob holds it OPEN)
//!                            ✗ primary killed HERE ✗
//!   B  alice ──REQ──▶ proxy ──▶ primary                 (the datagram is never read)
//!                            ✗ primary killed HERE ✗
//!
//!      alice ──REQ──▶ proxy ──▶ SURVIVOR ──REQ──▶ bob   (the peer re-sends at T1)
//!                            ◀── the survivor answers ──
//! ```
//!
//! UPDATE, INFO and the re-INVITE take shape **A**. BYE and CANCEL cannot: the
//! serving node answers a BYE in the same turn it begins the teardown, and it
//! answers a CANCEL from the INVITE server transaction it holds, so neither
//! leaves an A-window to kill inside. They take shape **B**, the kill landing on
//! the datagram itself.
//!
//! The peer's re-send is the datagram it already put on the wire, byte for byte,
//! addressed where it addressed the first copy — the first retransmission rung
//! after the kill (Timer E for a non-INVITE; Timer A for the re-INVITE, whose
//! client transaction is still `Calling` because the front proxy absorbs the
//! worker's 100 Trying, RFC 3261 §16.7) driven by hand, since these UAs run no
//! retransmit engine.
//!
//! What each cell pins: the survivor takes the call over on that re-send, and
//! the cluster settles to exactly one CDR with no trace and clean memory on both
//! nodes. The non-INVITE cells verify D7's "a relayed non-INVITE needs no server
//! seed — the peer's re-emission covers it": nothing on the survivor rebuilds
//! the server transaction the dead primary held, the re-send builds a fresh one,
//! is answered, and the call continues (UPDATE, INFO) or ends (BYE). The two
//! INVITE cells verify the seeds (D7, D9): the caller's CANCEL is answered from
//! the a-leg INVITE transaction the survivor rebuilds from the record, and the
//! caller's re-INVITE re-send is absorbed by the transaction the record names —
//! one b-leg INVITE for the round, resolved on the seeded transaction's bound,
//! the caller's own re-INVITE then drawing its final (RFC 3261 §15.1.2).
//!
//! The killed primary is the call's sole CDR authority (ADR-0020 X3), so every
//! cell reboots it and lets its reclaim discharge the terminal the survivor
//! deferred.
//!
//! A cell whose contract the cluster does not yet hold stays here, red, naming
//! the issue that owns it — the cell scripts the contract, never the defect.

use std::net::SocketAddr;
use std::time::Duration;

use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness, ProxySut,
    ReplicatedB2buaSut, WorkerHealth, RULE_CSEQ_IN_DIALOG_ORDER,
};
use scenario_harness::agent::{Agent, Dialog, Inbound};
use sip_message::generators::InDialogMethod;
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";
/// The INFO body the caller drives, and its media type — an arbitrary-MIME
/// payload, so the cell proves the body crosses the takeover too.
const INFO_CT: &str = "application/example-binary";
const INFO_BODY: &[u8] = b"SUP:role=agent;priority=high";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";

/// The first retransmission rung (RFC 3261 §17.1.1.2 / §17.1.2.2). The kill and
/// the re-send together consume it.
const T1: Duration = Duration::from_millis(500);
/// The pause held after the kill, before the peer's re-send. It is the window in
/// which a shape-**A** cell's doomed callee answer reaches the proxy and is
/// dropped there — its next Via names a dead node and an in-dialog response
/// carries no Record-Route — so the re-send is provably the first datagram that
/// can hydrate the call.
const KILL_SETTLE: Duration = Duration::from_millis(300);
/// How far the front proxy's health view lags a node's death. The doomed copy
/// crosses the proxy inside it and lands on the dead node's socket, which is
/// where a shape-**B** request is lost. `HEALTH_LAG + KILL_SETTLE == T1`, so the
/// peer's re-send still follows the kill by exactly one rung.
const HEALTH_LAG: Duration = Duration::from_millis(200);
/// The pause that lets a relayed round replicate before the kill, when the
/// cell's contract is what the survivor rebuilds from the record.
const REPLICATE: Duration = Duration::from_millis(500);

/// The INVITE bound the workers run under (the cells tune none) — the give-up
/// of a seeded client INVITE, measured from the seed.
fn invite_bound() -> Duration {
    Duration::from_secs(b2bua::B2buaConfig::default().invite_txn_timeout_sec as u64)
}

/// Why `cseq-in-dialog-order` is accepted from the kill onward in every cell.
const CSEQ_OVERLAP: &str = "ADR-0014 accepted trade-off: with two potential owners of one leg in the \
                            takeover window, the b-leg CSeq the dead primary minted may be minted \
                            again by the survivor";

/// The proxy plus both workers, ready at steady state.
async fn bring_up(fh: &mut FailoverHarness) -> (ProxySut, ReplicatedB2buaSut, ReplicatedB2buaSut) {
    let proxy = fh
        .spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())])
        .await;
    let w_b1 = fh
        .spawn_worker("b1", "b1", B1, &["b2"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    let w_b2 = fh
        .spawn_worker("b2", "b2", B2, &["b1"], ("127.0.0.1", 5070), ("127.0.0.1", 5080))
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both workers ready at steady state");
    (proxy, w_b1, w_b2)
}

/// Establish alice ⇄ bob through the proxy; returns the confirmed dialog and the
/// ordinal of the worker the proxy's stickiness cookie placed the call on.
async fn establish(fh: &FailoverHarness, alice: &Agent, bob: &Agent, proxy: &ProxySut) -> (Dialog, String) {
    let mut call = alice.invite(bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    // Let the established call replicate before anything else happens.
    fh.advance(Duration::from_millis(500)).await;
    (dialog, pri_ord)
}

/// The exact datagram `from` last put on the wire for `method`, with the
/// destination it addressed — the peer's own copy, which it re-sends verbatim
/// when nothing answers.
fn last_request_sent(fh: &FailoverHarness, from: SocketAddr, method: &Method) -> (Vec<u8>, SocketAddr) {
    let parser = CustomParser::new();
    fh.sip_entries()
        .into_iter()
        .filter(|e| e.from == from)
        .rfind(|e| matches!(parser.parse(&e.raw), Ok(SipMessage::Request(r)) if r.method() == method))
        .map(|e| (e.raw, e.to))
        .unwrap_or_else(|| panic!("{from} put a {method} on the wire this run"))
}

/// Abort the node serving the call with the peer's request unanswered, and open
/// the RFC deviation window the takeover carries. Returns the survivor's
/// takeover count before the kill.
fn crash_serving_node(
    fh: &mut FailoverHarness,
    primary: &mut ReplicatedB2buaSut,
    survivor: &ReplicatedB2buaSut,
    pri_ord: &str,
) -> u64 {
    fh.accept_rfc_deviations_from_now(RULE_CSEQ_IN_DIALOG_ORDER, CSEQ_OVERLAP);
    fh.mark(pri_ord, None, "crash", "serving node down with the peer's request in flight");
    let hydrated_before = survivor.metrics().repl_takeover_hydrated_total();
    primary.crash();
    hydrated_before
}

/// The shape-**A** kill: the callee holds the exchange open, so the cluster is
/// told at once — the proxy stops routing to the dead node, the survivor drops
/// it from membership.
async fn kill_serving_node(
    fh: &mut FailoverHarness,
    primary: &mut ReplicatedB2buaSut,
    survivor: &ReplicatedB2buaSut,
    pri_ord: &str,
    proxy: &ProxySut,
) -> u64 {
    let hydrated_before = crash_serving_node(fh, primary, survivor, pri_ord);
    proxy.set_health(pri_ord, WorkerHealth::Dead);
    survivor.simulate_peer_removed(pri_ord);
    fh.advance(KILL_SETTLE).await;
    hydrated_before
}

/// The shape-**B** kill: the node dies with the peer's datagram still on the
/// fabric. The proxy's health view lags, so it forwards the doomed copy onto the
/// dead node's socket — that is where the request is lost — and only then stops
/// routing there.
async fn kill_under_the_datagram(
    fh: &mut FailoverHarness,
    primary: &mut ReplicatedB2buaSut,
    survivor: &ReplicatedB2buaSut,
    pri_ord: &str,
    proxy: &ProxySut,
) -> u64 {
    let hydrated_before = crash_serving_node(fh, primary, survivor, pri_ord);
    fh.advance(HEALTH_LAG).await;
    proxy.set_health(pri_ord, WorkerHealth::Dead);
    survivor.simulate_peer_removed(pri_ord);
    fh.advance(KILL_SETTLE).await;
    hydrated_before
}

/// The node that did not serve the call, of the two.
fn survivor_of<'a>(
    pri_ord: &str,
    w_b1: &'a ReplicatedB2buaSut,
    w_b2: &'a ReplicatedB2buaSut,
) -> &'a ReplicatedB2buaSut {
    if pri_ord == "b1" {
        w_b2
    } else {
        w_b1
    }
}

/// The node that serves the call and the node that does not, with the serving one
/// mutable — the kill and the reboot act on it.
fn split_mut<'a>(
    pri_ord: &str,
    w_b1: &'a mut ReplicatedB2buaSut,
    w_b2: &'a mut ReplicatedB2buaSut,
) -> (&'a mut ReplicatedB2buaSut, &'a ReplicatedB2buaSut) {
    if pri_ord == "b1" {
        (w_b1, &*w_b2)
    } else {
        (w_b2, &*w_b1)
    }
}

/// The steady state every cell starts from, read back from the cluster: the call
/// is replicated, the node the stickiness cookie picked serves it, and the other
/// node is a synchronized backup (holds the replica, serves nothing). Returns the
/// cluster's `call_ref`.
async fn synchronized_call_ref(
    pri_ord: &str,
    w_b1: &ReplicatedB2buaSut,
    w_b2: &ReplicatedB2buaSut,
) -> String {
    let primary = if pri_ord == "b1" { w_b1 } else { w_b2 };
    let survivor = survivor_of(pri_ord, w_b1, w_b2);
    let call_ref = survivor
        .scan_one_backed_up(pri_ord)
        .await
        .expect("the call replicated to the backup");
    assert!(primary.serves(&call_ref), "the primary serves the call");
    assert!(
        survivor.is_synchronized_backup(&call_ref).await,
        "the backup is synchronized (holds the replica, serves nothing)",
    );
    call_ref
}

/// How many responses to `method` were put on the wire toward `to` — the peer's
/// own view of whether its exchange was ever answered.
fn responses_toward(fh: &FailoverHarness, to: SocketAddr, method: &Method) -> usize {
    let parser = CustomParser::new();
    fh.sip_entries()
        .into_iter()
        .filter(|e| e.to == to)
        .filter(|e| {
            matches!(parser.parse(&e.raw), Ok(SipMessage::Response(r))
                if r.cseq().method() == method)
        })
        .count()
}

/// The kill landed inside the in-flight window and nothing since has moved the
/// call: the survivor serves nothing for it and its hydration count is where the
/// kill left it. Asserted immediately before the re-send, so the takeover the
/// cell then observes can only be the re-send's.
fn assert_not_taken_over(survivor: &ReplicatedB2buaSut, hydrated_before: u64, call_ref: &str) {
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated_before,
        "nothing has taken the call over on the survivor yet",
    );
    assert!(!survivor.serves(call_ref), "the survivor serves nothing for the call yet");
}

/// The peer's re-send is what took the call over, and it did so once — a second
/// hydration would mean the copy was released and rebuilt inside the window.
fn assert_taken_over_once(survivor: &ReplicatedB2buaSut, hydrated_before: u64) {
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated_before + 1,
        "the peer's re-send hydrated the call onto the survivor, exactly once",
    );
}

/// How many INVITE transactions carrying `offer` were put on the wire toward
/// `to`, delivered or not — the b-leg INVITEs the cluster minted for one
/// renegotiation round, counted on the recorded wire by top-Via branch: a Timer
/// A rung repeats the transaction it belongs to (RFC 3261 §17.1.1.2) and is not
/// a second INVITE.
fn round_invites_sent_to(fh: &FailoverHarness, to: SocketAddr, offer: &str) -> usize {
    let parser = CustomParser::new();
    fh.sip_entries()
        .into_iter()
        .filter(|e| e.to == to)
        .filter_map(|e| match parser.parse(&e.raw) {
            Ok(SipMessage::Request(r)) if *r.method() == Method::Invite && r.body() == offer.as_bytes() => {
                r.top_via().branch().map(str::to_string)
            }
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>()
        .len()
}

/// Reboot the crashed primary empty at a higher gen + new pod IP, re-learn its
/// address, and let its go-active reclaim discharge the terminal the survivor
/// deferred (ADR-0020 X3).
async fn reboot_and_reclaim(
    fh: &mut FailoverHarness,
    primary: &mut ReplicatedB2buaSut,
    survivor: &ReplicatedB2buaSut,
    pri_ord: &str,
    proxy: &ProxySut,
) {
    fh.mark(pri_ord, None, "reboot", "restart empty, higher gen, new pod IP");
    let new_addr = primary.reboot().await;
    proxy.set_address(pri_ord, new_addr);
    fh.note_worker_rebound(pri_ord, new_addr);
    survivor.simulate_peer_added(pri_ord);
    for _ in 0..120 {
        fh.advance(Duration::from_millis(500)).await;
        if primary.is_ready() {
            break;
        }
    }
    assert!(primary.is_ready(), "rebooted primary {pri_ord} became ready");
    proxy.set_health(pri_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_secs(10)).await;
}

/// The cluster post-condition every cell ends on: settled, exactly one CDR, no
/// trace and clean per-call memory on both nodes.
async fn settle_to_one_cdr(
    fh: &FailoverHarness,
    peers: &[&Agent],
    nodes: &[&ReplicatedB2buaSut; 2],
    call_ref: &str,
) {
    let drained = fh
        .settle_terminal(async || {
            nodes[0].memory_clean()
                && nodes[1].memory_clean()
                && !nodes[0].holds_any_trace(call_ref).await
                && !nodes[1].holds_any_trace(call_ref).await
        })
        .await;
    assert!(drained, "the cluster drains the failed-over call within the settle budget");
    fh.linger_peers(peers, Duration::from_secs(3)).await;
    assert_eq!(
        total_cdrs_for(&nodes[..], call_ref),
        1,
        "exactly one CDR for the failed-over call across the cluster",
    );
    assert_call_fully_released(&nodes[..], call_ref).await;
}

/// An in-dialog **UPDATE** (RFC 3311) relayed to the callee and still unanswered
/// when the serving node dies: the caller's own re-send takes the call over on
/// the survivor, which relays it and answers, and the session lives on.
#[tokio::test(start_paused = true)]
async fn an_update_in_flight_at_the_kill_is_served_on_the_peers_resend() {
    let mut fh = FailoverHarness::new("inflight-resend-update", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;

    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    // ── The UPDATE goes IN FLIGHT: the primary relayed it, nobody answered ───
    let mut update = dialog
        .send_request(InDialogMethod::Update)
        .with_sdp(REOFFER)
        .send()
        .await;
    let mut first_at_bob = bob.receive("UPDATE").await;

    // ── The kill lands inside that window ───────────────────────────────────
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    // The callee answers as a UAS must; the response is undeliverable — the node
    // whose transaction it belongs to is gone.
    first_at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    fh.advance(T1 - KILL_SETTLE).await;
    assert_not_taken_over(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before, &call_ref);

    // ── The peer re-sends its own datagram at T1 ────────────────────────────
    let (wire, dst) = last_request_sent(&fh, alice.addr(), &Method::Update);
    alice.try_send_datagram(&wire, dst).await.expect("the peer's re-send leaves");

    let mut second_at_bob = bob.receive("UPDATE").await;
    second_at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    // The survivor answers the re-sent UPDATE (`expect` fails on any other status).
    update.expect(200).await;
    fh.advance(Duration::from_millis(200)).await;
    assert_taken_over_once(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before);

    // ── The call lives on and is hung up normally ───────────────────────────
    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// An in-dialog **INFO** relayed to the callee and still unanswered when the
/// serving node dies: the caller's own re-send takes the call over on the
/// survivor, body and all, and the session lives on.
#[tokio::test(start_paused = true)]
async fn an_info_in_flight_at_the_kill_is_served_on_the_peers_resend() {
    let mut fh = FailoverHarness::new("inflight-resend-info", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;

    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    // ── The INFO goes IN FLIGHT: the primary relayed it, nobody answered ─────
    let mut info = dialog
        .send_request(InDialogMethod::Info)
        .with_body(INFO_CT, INFO_BODY.to_vec())
        .send()
        .await;
    let mut first_at_bob = bob.receive("INFO").await;

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    first_at_bob.respond(200, "OK").await;
    fh.advance(T1 - KILL_SETTLE).await;
    assert_not_taken_over(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before, &call_ref);

    let (wire, dst) = last_request_sent(&fh, alice.addr(), &Method::Info);
    alice.try_send_datagram(&wire, dst).await.expect("the peer's re-send leaves");

    let mut second_at_bob = bob.receive("INFO").await;
    assert_eq!(
        second_at_bob.request().body(),
        INFO_BODY,
        "the survivor relays the re-sent INFO's body onward unchanged",
    );
    second_at_bob.respond(200, "OK").await;
    // The survivor answers the re-sent INFO (`expect` fails on any other status).
    info.expect(200).await;
    fh.advance(Duration::from_millis(200)).await;
    assert_taken_over_once(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before);

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// A caller's **BYE** unanswered when the serving node dies: the caller's own
/// re-send takes the call over on the survivor, which completes the teardown —
/// one CDR, nothing left anywhere.
///
/// Shape **B**: answering a BYE is one turn on the serving node (the a-leg 200
/// and the b-leg BYE leave together), so the only window in which the caller's
/// BYE is unanswered is its transit — the node dies with the datagram still on
/// the fabric, and the proxy hands it to a socket nothing reads.
#[tokio::test(start_paused = true)]
async fn a_bye_in_flight_at_the_kill_is_served_on_the_peers_resend() {
    let mut fh = FailoverHarness::new("inflight-resend-bye", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;

    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    // ── The BYE goes IN FLIGHT: sent, then the node dies under it ───────────
    let mut bye = dialog.bye().await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before =
        kill_under_the_datagram(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    assert_eq!(
        responses_toward(&fh, alice.addr(), &Method::Bye),
        0,
        "nothing answered the caller's BYE: the kill landed on the datagram itself",
    );
    assert_not_taken_over(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before, &call_ref);

    let (wire, dst) = last_request_sent(&fh, alice.addr(), &Method::Bye);
    alice.try_send_datagram(&wire, dst).await.expect("the peer's re-send leaves");

    let mut second_at_bob = bob.receive("BYE").await;
    second_at_bob.respond(200, "OK").await;
    // Nothing answered the first copy, so this 200 is the survivor's.
    bye.expect(200).await;
    fh.advance(Duration::from_millis(200)).await;
    assert_taken_over_once(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before);

    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop((dialog, proxy));
}

/// A **re-INVITE** relayed to the callee and still unanswered when the serving
/// node dies: the caller's own re-send takes the call over on the survivor and
/// is absorbed as the retransmission it is — the round's relayed INVITE is
/// seeded from the replicated snapshot, so exactly one b-leg INVITE exists for
/// the round. The callee's final for that INVITE is addressed to the dead node
/// and can never be redelivered, so the seeded client transaction resolves the
/// round on its own bound, and the survivor then ends the dialog it got no
/// answer on (RFC 3261 §12.2.1.2): the caller's pending re-INVITE draws its
/// 487 (§15.1.2) and the callee's a CANCEL, both parties draw a BYE, and the
/// call ends with one CDR and full release on both nodes.
///
/// The round must be in the replica before the kill: a round the dead node
/// never replicated is a state change inside the acceptance window, the
/// accepted collateral (ADR-0014), not this cell's contract. So the kill lands
/// after the relay has replicated, and the re-send is the caller's next rung —
/// the first one after the kill.
#[tokio::test(start_paused = true)]
async fn a_reinvite_in_flight_at_the_kill_is_served_on_the_peers_resend() {
    let mut fh = FailoverHarness::new("inflight-resend-reinvite", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;

    let (mut dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    // ── The re-INVITE goes IN FLIGHT: relayed, replicated, nobody answered ──
    let reinvite = dialog
        .send_request(InDialogMethod::Invite)
        .with_sdp(REOFFER)
        .send()
        .await;
    let mut first_at_bob = bob.receive("INVITE").await;
    fh.advance(REPLICATE).await;

    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before = kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    // The callee answers as a UAS must; the 2xx is undeliverable and its
    // transaction died with the node that opened it. The dialog's own teardown
    // below is what clears it (RFC 3261 §13.3.1.4).
    first_at_bob.respond(200, "OK").with_sdp(REANSWER).await;
    // Timer A's rungs fall at T1 and 3·T1 after the re-INVITE; the kill sits
    // between them, so the next rung is the first re-send after it.
    fh.advance(3 * T1 - REPLICATE - KILL_SETTLE).await;
    assert_not_taken_over(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before, &call_ref);

    // ── The peer re-sends its own datagram at its next rung (T1 + 2·T1) ─────
    let (wire, dst) = last_request_sent(&fh, alice.addr(), &Method::Invite);
    alice.try_send_datagram(&wire, dst).await.expect("the peer's re-send leaves");
    fh.advance(Duration::from_millis(200)).await;
    assert_taken_over_once(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before);
    assert_eq!(
        round_invites_sent_to(&fh, bob.addr(), REOFFER),
        1,
        "exactly one b-leg INVITE exists for the renegotiation round",
    );

    // ── The round resolves on the seeded transaction's bound ────────────────
    // Nothing further reaches the survivor for the round, so its seeded client
    // INVITE gives up on the configured INVITE bound, and the survivor ends the
    // dialog it got no answer on (RFC 3261 §12.2.1.2). The caller's re-INVITE
    // is still pending on the survivor: it draws its final first (§15.1.2),
    // then the BYE.
    fh.advance(invite_bound() + Duration::from_millis(500)).await;
    let round_final = loop {
        match alice.recv_any().await.expect("the caller draws a response to its re-INVITE") {
            Inbound::Response(r) if r.status() < 200 => continue,
            Inbound::Response(r) => break r,
            Inbound::Request(txn) => panic!(
                "the caller expected a final for its re-INVITE, got a {} request",
                txn.request().method(),
            ),
        }
    };
    assert_eq!(round_final.status(), 487, "the pending re-INVITE ends with the dialog (RFC 3261 §15.1.2)");
    reinvite.ack_non_2xx(&round_final).await.expect("the caller hop-ACKs the 487");
    alice.receive("BYE").await.respond(200, "OK").await;
    // The callee's side of the round is CANCELled (§9.2: a CANCEL of an INVITE
    // it already answered draws 200 and changes nothing), then its dialog ends.
    bob.receive("CANCEL").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    fh.advance(Duration::from_millis(500)).await;

    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}

/// A caller's **CANCEL** of a ringing call, in flight when the serving node
/// dies: the caller's own re-send takes the call over on the survivor, which
/// answers the CANCEL from the a-leg INVITE transaction it seeds from the
/// replicated snapshot, cancels the b-leg and gives the caller its 487 under
/// the To-tag the 180 gave its early dialog (RFC 3261 §17.2.1).
#[tokio::test(start_paused = true)]
async fn a_cancel_in_flight_at_the_kill_is_served_on_the_peers_resend() {
    let mut fh = FailoverHarness::new("inflight-resend-cancel", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;

    // ── A ringing call: the primary serves it, the backup is synchronized ────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak) = worker_ordinals(uas.request());
    uas.respond(180, "Ringing").await;
    let ringing = call.expect(180).await;
    fh.advance(Duration::from_millis(500)).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    // ── The caller gives up; the node dies with the CANCEL still on the fabric,
    //    so the doomed copy lands on a dead socket and nothing ever answers it ─
    let mut cancel = call.cancel().await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    let hydrated_before =
        kill_under_the_datagram(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    assert_eq!(
        responses_toward(&fh, alice.addr(), &Method::Cancel),
        0,
        "nothing answered the caller's CANCEL: the kill landed on the datagram itself",
    );
    assert_not_taken_over(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before, &call_ref);

    // ── The peer re-sends its own datagram at T1 ────────────────────────────
    let (wire, dst) = last_request_sent(&fh, alice.addr(), &Method::Cancel);
    alice.try_send_datagram(&wire, dst).await.expect("the peer's re-send leaves");

    // The survivor answers the caller's CANCEL (RFC 3261 §9.2).
    cancel.expect(200).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;
    // The cancelled INVITE resolves at the caller.
    let terminated = call.expect(487).await;
    assert_eq!(
        terminated.to().tag(),
        ringing.to().tag(),
        "the survivor's 487 rides the To-tag the dead node's 180 pinned (RFC 3261 §17.2.1)",
    );
    fh.advance(Duration::from_millis(200)).await;
    assert_taken_over_once(survivor_of(&pri_ord, &w_b1, &w_b2), hydrated_before);

    fh.advance(Duration::from_secs(60)).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    drop(proxy);
}
