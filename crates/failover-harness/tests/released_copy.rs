//! **A datagram that arrives after the takeover copy has been released.**
//!
//! A backup that takes a call over sheds its live copy once the transaction(s)
//! it served quiesce, reverse-flushing the terminal body so the primary folds or
//! reclaims it (ADR-0014, ADR-0020 X3). That retained body is the image of a call
//! that has ENDED — it is not a call to serve. Anything still addressing the
//! dialog after the shed must be answered as a call that no longer exists:
//!
//! ```text
//!   established · primary serves · backup synchronized
//!                    ✗ primary killed ✗
//!   alice ──BYE──▶ proxy ──▶ survivor  (takeover: hydrate, tear down, self-release)
//!   ... the released copy's Terminated body stays in bak:{primary} ...
//!   bob ──BYE──▶ proxy ──▶ survivor ──▶ 481          (a REQUEST is refused)
//!   bob ──200───▶            survivor ──▶ (nothing)  (a RESPONSE is dropped)
//! ```
//!
//! Both cells assert the same thing twice over: the survivor does not take the
//! call over a second time (`repl_takeover_hydrated_total` does not move, and the
//! takeover log carries one hydration for the whole run), it counts the refusal
//! instead, and the cluster still settles to exactly one CDR with no trace and
//! clean memory on both nodes.

use std::net::SocketAddr;
use std::time::Duration;

use failover_harness::{
    assert_call_fully_released, total_cdrs_for, worker_ordinals, FailoverHarness, ProxySut,
    ReplicatedB2buaSut, WorkerHealth, RULE_CSEQ_IN_DIALOG_ORDER,
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

/// Why `cseq-in-dialog-order` is accepted from the kill onward: with two
/// potential owners of one leg in the takeover window, the b-leg CSeq the dead
/// primary minted may be minted again by the survivor (ADR-0014).
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

/// Establish alice ⇄ bob through the proxy. Returns both parties' confirmed
/// dialogs (the callee owns one too — it is the party that speaks last here) and
/// the ordinal of the worker the proxy's stickiness cookie placed the call on.
async fn establish(
    fh: &FailoverHarness,
    alice: &Agent,
    bob: &Agent,
    proxy: &ProxySut,
) -> (Dialog, Dialog, String) {
    let mut call = alice.invite(bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let (pri_ord, _bak) = worker_ordinals(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    bob.receive("ACK").await;
    let bob_dialog = uas.dialog();
    // Let the established call replicate before anything else happens.
    fh.advance(Duration::from_millis(500)).await;
    (dialog, bob_dialog, pri_ord)
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

/// The steady state both cells start from, read back from the cluster: the call
/// is replicated, the node the stickiness cookie picked serves it, and the other
/// node is a synchronized backup (holds the replica, serves nothing).
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

/// Abort the node serving the call and tell the cluster, opening the RFC
/// deviation window the takeover carries.
async fn kill_serving_node(
    fh: &mut FailoverHarness,
    primary: &mut ReplicatedB2buaSut,
    survivor: &ReplicatedB2buaSut,
    pri_ord: &str,
    proxy: &ProxySut,
) {
    fh.accept_rfc_deviations_from_now(RULE_CSEQ_IN_DIALOG_ORDER, CSEQ_OVERLAP);
    fh.mark(pri_ord, None, "crash", "serving node down on an established call");
    primary.crash();
    proxy.set_health(pri_ord, WorkerHealth::Dead);
    survivor.simulate_peer_removed(pri_ord);
    fh.advance(Duration::from_millis(300)).await;
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

/// The cluster post-condition both cells end on: settled, exactly one CDR, no
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

/// The takeover events the log carries: a `rising` event opens an episode keyed
/// by the dead peer, its 5 s `summary` events report the running totals, and a
/// `falling` event closes it on the episode's totals.
fn takeover_events(log: &observe::TestLogHandle) -> Vec<observe::CapturedEvent> {
    log.matching("acting-backup takeover")
}

/// The rendered value of `e`'s `name` field (empty when it carries none).
fn field<'a>(e: &'a observe::CapturedEvent, name: &str) -> &'a str {
    e.fields.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str()).unwrap_or_default()
}

/// The value `e`'s `totals` field carries for `counter` (`0` when absent): the
/// field is the `name=n` run a `Tally` renders.
fn tallied(e: &observe::CapturedEvent, counter: &str) -> u64 {
    field(e, "totals")
        .split_whitespace()
        .find_map(|pair| pair.strip_prefix(counter)?.strip_prefix('=')?.parse().ok())
        .unwrap_or(0)
}

/// What the takeover log says `counter` did over the whole run: an episode
/// carries its totals on its falling edge, so the run's total is the sum over
/// closed episodes. Read it only behind [`assert_every_episode_closed`].
fn logged_total(log: &observe::TestLogHandle, counter: &str) -> u64 {
    takeover_events(log)
        .iter()
        .filter(|e| field(e, "edge") == "falling")
        .map(|e| tallied(e, counter))
        .sum()
}

/// Every takeover episode the run opened has also closed, so the falling edges
/// carry the whole run's totals. A timing regression that leaves an episode open
/// fails here, on the episode, rather than as a missing counter downstream.
fn assert_every_episode_closed(log: &observe::TestLogHandle) {
    let events = takeover_events(log);
    let opened = events.iter().filter(|e| field(e, "edge") == "rising").count();
    let closed = events.iter().filter(|e| field(e, "edge") == "falling").count();
    let lines = rendered(&events);
    assert_eq!(opened, closed, "every takeover episode opened must have closed: {lines:?}");
}

/// The takeover events as log lines, for an assertion message.
fn rendered(events: &[observe::CapturedEvent]) -> Vec<String> {
    events.iter().map(observe::CapturedEvent::line).collect()
}

/// The log plane says the same as the counters: the call was taken over ONCE,
/// and the datagram that arrived after the release was refused, not served.
fn assert_one_hydration_then_a_refusal(log: &observe::TestLogHandle) {
    assert_every_episode_closed(log);
    let lines = rendered(&takeover_events(log));
    assert_eq!(
        logged_total(log, "hydrated"),
        1,
        "the takeover log must carry ONE hydration for the call, never a second \
         one for the released copy: {lines:?}",
    );
    assert_eq!(
        logged_total(log, "refused_terminated"),
        1,
        "the takeover log records the refusal that replaced it: {lines:?}",
    );
}

/// The exact datagram `from` last put on the wire as a response with CSeq
/// `method`, and the destination it addressed.
fn last_response_sent(
    fh: &FailoverHarness,
    from: SocketAddr,
    method: &Method,
) -> (Vec<u8>, SocketAddr) {
    let parser = CustomParser::new();
    fh.sip_entries()
        .into_iter()
        .filter(|e| e.from == from)
        .rfind(|e| {
            matches!(parser.parse(&e.raw), Ok(SipMessage::Response(r))
                if r.cseq().method() == method)
        })
        .map(|e| (e.raw, e.to))
        .unwrap_or_else(|| panic!("{from} answered a {method} on the wire this run"))
}

/// How many datagrams were put on the wire toward `to` — the peer's own view of
/// whether anything answered it.
fn datagrams_toward(fh: &FailoverHarness, to: SocketAddr) -> usize {
    fh.sip_entries().into_iter().filter(|e| e.to == to).count()
}

/// A **request** that names a takeover copy the survivor already released draws
/// `481`: the caller's BYE is served on the survivor, which sheds the copy, and
/// the callee's redundant terminal — a second BYE for the dialog it has already
/// ended — finds no call and is told so. The survivor does not take the ended
/// call over a second time.
#[tokio::test(start_paused = true)]
async fn a_late_request_naming_a_released_takeover_copy_draws_481() {
    let (_log_guard, log) = observe::test_buffer();
    let mut fh = FailoverHarness::new("released-copy-late-request", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;

    let (mut dialog, mut bob_dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    // ── The serving node dies; the caller hangs up onto the survivor ────────
    {
        let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
        kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    }
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    let hydrated_before = survivor.metrics().repl_takeover_hydrated_total();
    let refused_before = survivor.metrics().repl_takeover_refused_terminated_total();

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_millis(500)).await;

    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated_before + 1,
        "the caller's BYE took the call over on the survivor, exactly once",
    );
    assert!(
        fh.settle_terminal(async || !survivor.serves(&call_ref)).await,
        "the takeover copy releases once the teardown it served quiesced",
    );

    // ── The late request: the callee's own terminal for the same dialog ─────
    let mut bye_b = bob_dialog.bye().await;
    bye_b.expect_tolerating(481, &["OPTIONS"]).await;
    fh.advance(Duration::from_millis(200)).await;

    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated_before + 1,
        "the released copy was NOT taken over again by the late BYE",
    );
    assert_eq!(
        survivor.metrics().repl_takeover_refused_terminated_total(),
        refused_before + 1,
        "the survivor refused the ended call's replica instead of serving it",
    );

    // ── The primary returns and discharges the deferred terminal ────────────
    fh.advance(Duration::from_secs(60)).await;
    {
        let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
        reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    }
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    assert_one_hydration_then_a_refusal(&log);
    drop(proxy);
}

/// A **response** that names a takeover copy the survivor already released is
/// dropped, not answered: the callee's 200 to the b-leg BYE is duplicated on the
/// fabric and arrives once the survivor's client transaction for that BYE is
/// gone — cancelled with the release, or retired by Timer K, whichever comes
/// first — so it reaches the call layer with no call to give it to. Nothing goes
/// back to the callee and the ended call is not taken over again.
#[tokio::test(start_paused = true)]
async fn a_late_response_naming_a_released_takeover_copy_is_dropped() {
    let (_log_guard, log) = observe::test_buffer();
    let mut fh = FailoverHarness::new("released-copy-late-response", &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let (proxy, mut w_b1, mut w_b2) = bring_up(&mut fh).await;

    let (mut dialog, _bob_dialog, pri_ord) = establish(&fh, &alice, &bob, &proxy).await;
    let call_ref = synchronized_call_ref(&pri_ord, &w_b1, &w_b2).await;

    {
        let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
        kill_serving_node(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    }
    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    let hydrated_before = survivor.metrics().repl_takeover_hydrated_total();
    let refused_before = survivor.metrics().repl_takeover_refused_terminated_total();

    scenario_harness::callflow::hangup(&mut dialog, &bob).await;
    fh.advance(Duration::from_millis(500)).await;

    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated_before + 1,
        "the caller's BYE took the call over on the survivor, exactly once",
    );
    assert!(
        fh.settle_terminal(async || !survivor.serves(&call_ref)).await,
        "the takeover copy releases once the teardown it served quiesced",
    );

    // ── The duplicate arrives with no transaction left to absorb it ─────────
    let (wire, dst) = last_response_sent(&fh, bob.addr(), &Method::Bye);
    fh.advance(Duration::from_secs(10)).await;
    let before = datagrams_toward(&fh, bob.addr());
    bob.try_send_datagram(&wire, dst).await.expect("the duplicated 200 leaves");
    fh.advance(Duration::from_millis(500)).await;

    let survivor = survivor_of(&pri_ord, &w_b1, &w_b2);
    assert_eq!(
        datagrams_toward(&fh, bob.addr()),
        before,
        "a response for an ended call is dropped: nothing goes back to the callee",
    );
    assert_eq!(
        survivor.metrics().repl_takeover_hydrated_total(),
        hydrated_before + 1,
        "the released copy was NOT taken over again by the late 200",
    );
    assert_eq!(
        survivor.metrics().repl_takeover_refused_terminated_total(),
        refused_before + 1,
        "the survivor refused the ended call's replica instead of serving it",
    );

    fh.advance(Duration::from_secs(60)).await;
    {
        let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
        reboot_and_reclaim(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    }
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    assert_one_hydration_then_a_refusal(&log);
    drop(proxy);
}
