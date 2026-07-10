//! SIP-stack **limit cases**, each verified end-to-end through the single-SUT
//! B2BUA harness with a *real* call-limiter wired in. The shared concern across
//! every test here is the one the [`crossing`](super) race tests share — a
//! corner that, mishandled, **leaks** per-call state. The specific leak this
//! suite pins shut is the **limiter hold**: every admitted call takes a slot at
//! route time, and EVERY limit teardown path (max-duration BYE, message-cap
//! 503, the crossing CANCEL/200) MUST release it — `current_total` back to 0 —
//! exactly as a clean BYE would. A limit that tears the call down but forgets
//! the limiter decrement pins a trunk's capacity forever (the endurance cap20
//! pinning class, see [[stuck-setup-zombie-limiter-pinning]]).
//!
//! Coverage map (the user's enumerated limit cases):
//!   - ring forever / no-answer → setup timeout: ALREADY covered with a limiter
//!     in `setup_timeout.rs::ringing_forever_is_torn_down_at_setup_timeout_and_
//!     releases_the_limiter` (150 s a-leg deadline; >180 s is unreachable — the
//!     sip-txn `INVITE_INITIAL_TIMEOUT` backstop is 158 s, below 180). Not
//!     re-implemented here; see that file + the doc on the no-answer test below.
//!   - call never hangs up → max-duration BYE: `max_duration_byes_both_legs_*`.
//!   - too many 18x before connect → message cap: `provisional_storm_*`.
//!   - too many in-dialog messages on an up call → message cap: `in_dialog_*`.
//! The 200/CANCEL crossing limit case lives in its sibling `cancel_200_crossing.rs`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::cdr::CdrRecord;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallLimiterEntry, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use b2bua_harness::{settle_until, B2buaSut};
use call_limiter::{LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use scenario_harness::Harness;
use sip_clock::Clock;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 0\r\n";

const LIMITER_ADDR: &str = "10.0.0.1:8080";

fn laddr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

/// Stand up a real `LimiterServer` on its own simulated HTTP fabric. The
/// returned `WindowStore` is the ground-truth counter every test asserts drains
/// back to 0; the handle keeps the server task alive for the scenario.
async fn serve_limiter(net: &SimulatedHttpNetwork) -> (Arc<WindowStore>, Box<dyn HttpServerHandle>) {
    let store = Arc::new(WindowStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let handle = net.serve(laddr(), server).await.unwrap();
    (store, handle)
}

fn limiter_client(net: &SimulatedHttpNetwork) -> Arc<dyn CallLimiter> {
    Arc::new(HttpCallLimiter::new(
        Arc::new(net.clone()),
        laddr(),
        Duration::from_millis(150),
    ))
}

/// A decision that routes every call to `host:port`, takes a limiter hold under
/// `id` (capacity `limit`), and arms the call's `GlobalDuration` at
/// `max_duration_sec` (the per-call `features.platform.max_duration_sec` the
/// `max-duration` rule reads — see `rules/defaults.rs`).
fn route_limited(
    host: &str,
    port: u16,
    id: &str,
    limit: i64,
    max_duration_sec: i64,
) -> Arc<dyn CallDecisionEngine> {
    let host = host.to_string();
    let id = id.to_string();
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to(&host, port);
                r.call_limiter = vec![CallLimiterEntry { id: id.clone(), limit }];
                r.features.platform.max_duration_sec = max_duration_sec;
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

fn reasons_of(cdr: &CdrRecord) -> Vec<String> {
    cdr.events.iter().filter_map(|e| e.reason.clone()).collect()
}

/// **Call never hangs up → max-duration cap.** A perfectly healthy, established
/// call that simply never sends a BYE must be torn down at the absolute
/// `GlobalDuration` cap: the B2BUA BYEs *both* legs, writes the `max_duration`
/// CDR, and — the leak this suite guards — releases its limiter hold. Pre-fix a
/// "call that never hangs" would hold its trunk slot until the process died.
#[tokio::test(start_paused = true)]
async fn max_duration_byes_both_legs_and_releases_the_limiter() {
    let h = Harness::new("b2bua-limit-max-duration");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let http = SimulatedHttpNetwork::new();
    let (store, _limiter_srv) = serve_limiter(&http).await;
    // 60 s max-duration so the GlobalDuration trips quickly under the paused
    // clock; keepalive pushed far out and the reaper off so the ONLY thing that
    // can resolve this call is the GlobalDuration timer (isolating the cap).
    let decision = route_limited("127.0.0.1", 5070, "trunk-A", 1, 60);
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter_client(&http))
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    // ── establish ────────────────────────────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let _dialog = call.ack().await;
    bob.receive("ACK").await;

    assert_eq!(store.stats().current_total, 1, "established call holds one limiter slot");

    // ── never hang up; cross the 60 s absolute cap ───────────────────────────
    h.advance(Duration::from_secs(61)).await;

    // The B2BUA BYEs both legs at the cap (independent queues, any order).
    let mut a_bye = alice.receive("BYE").await;
    a_bye.respond(200, "OK").await;
    let mut b_bye = bob.receive("BYE").await;
    b_bye.respond(200, "OK").await;

    settle_until(|| store.stats().current_total == 0).await;
    assert_eq!(store.stats().current_total, 0, "limiter hold released at the max-duration teardown");
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1);
    assert!(
        reasons_of(&cdrs[0]).iter().any(|r| r.contains("max_duration")),
        "CDR carries the max-duration reason: {:?}",
        reasons_of(&cdrs[0]),
    );

    let _report = h.finish().await;
}

/// **Max-duration fires mid-re-INVITE.** The absolute `GlobalDuration` cap must
/// win over an in-flight re-negotiation: alice fires a re-INVITE (bob never
/// answers it), then the 60 s cap trips. The call is torn down — BYEs both legs,
/// abandons the pending re-INVITE — and the limiter hold is released. Guards
/// against a pending in-dialog transaction wedging the max-duration teardown.
#[tokio::test(start_paused = true)]
async fn max_duration_fires_mid_reinvite_and_releases_the_limiter() {
    let h = Harness::new("b2bua-limit-max-duration-reinvite");
    let alice = h.agent("alice", "127.0.0.1:5065").await;
    let bob = h.agent("bob", "127.0.0.1:5075").await;

    let http = SimulatedHttpNetwork::new();
    let (store, _limiter_srv) = serve_limiter(&http).await;
    // 10 s max-duration so GlobalDuration fires while the re-INVITE is still
    // pending — below the b-leg re-INVITE's 32 s transaction Timer B, which would
    // otherwise resolve the re-INVITE first.
    let decision = route_limited("127.0.0.1", 5075, "trunk-A", 1, 10);
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter_client(&http))
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5085")
        .await;

    // ── establish ────────────────────────────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    assert_eq!(store.stats().current_total, 1, "established call holds one limiter slot");

    // ── alice re-INVITEs; bob leaves it pending across the cap ───────────────
    let _reinv = dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let mut bob_reinv = bob.receive("INVITE").await;

    // ── the 10 s cap trips while the re-INVITE is pending ────────────────────
    h.advance(Duration::from_secs(11)).await;

    // The max-duration teardown BYEs both legs. The b-leg re-INVITE is abandoned
    // by the SUT (UAC); that client transaction still blocks call removal — and
    // hence the limiter release — until it completes. Resolve bob's open re-INVITE
    // server transaction with a 200 + SDP answer (RFC 3264 §5) so the client
    // transaction terminates. (Pre-newkahneed-034 this happened IMPLICITLY: the
    // Timer-A re-INVITE retransmit was 200-OK'd by `receive_tolerating("BYE",
    // &["INVITE"])`; now the §17.2 receive view absorbs the retransmit, so the
    // real answer must be sent explicitly — and a bodyless tolerated-200 to an
    // offer-carrying re-INVITE was an RFC 3264 §5 violation anyway.)
    bob_reinv.respond(200, "OK").with_sdp(ANSWER).await;
    let mut b_bye = bob.receive("BYE").await;
    b_bye.respond(200, "OK").await;
    // alice's queue holds the B2BUA's 100 Trying for her abandoned re-INVITE and
    // the a-leg BYE; drain it — her a-leg BYE then force-resolves at the 32 s
    // TerminatingTimeout backstop.
    alice.drain().await;
    h.advance(Duration::from_secs(33)).await;

    settle_until(|| store.stats().current_total == 0).await;
    assert_eq!(store.stats().current_total, 0, "limiter released at the max-duration teardown");
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    assert!(
        b2bua.cdr_records().iter().any(|c| reasons_of(c).iter().any(|r| r.contains("max_duration"))),
        "CDR carries the max-duration reason",
    );

    let _report = h.finish().await;
}

/// **Callee floods 18x before connect → message cap.** A b-leg that rings
/// without end — `> max_messages_per_call` (default 200) provisional responses
/// to the *initial* INVITE, no final — is a runaway dialog: each 180 allocates
/// a working clone + relay. The cap (router.rs cap-defense) must tear it down
/// mid-setup with the RFC-3326 503 cause and release the limiter hold taken at
/// route time. Provisional responses count (they are `SipMessage::Response`,
/// `initial_invite: false`), so the (default-200)+1'th 180 trips it.
#[tokio::test(start_paused = true)]
async fn provisional_storm_before_connect_trips_the_cap_and_releases_the_limiter() {
    let h = Harness::new("b2bua-limit-provisional-cap");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;

    let http = SimulatedHttpNetwork::new();
    let (store, _limiter_srv) = serve_limiter(&http).await;
    let decision = route_limited("127.0.0.1", 5071, "trunk-A", 1, 3_600);
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter_client(&http))
        .tune(|c| c.reaper_enabled = false)
        .start(&h, "b2bua", "127.0.0.1:5081")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    assert_eq!(store.stats().current_total, 1, "in-setup call holds one limiter slot");

    // Storm the b-leg with 180 Ringing — each counts one in-dialog event and is
    // relayed to alice — until the cap (default 200) trips. The tripping
    // provisional is itself still relayed (the in-flight event is serviced before
    // teardown), so `expect(180)` succeeds even on the round that flips the
    // metric; the 503 then follows in alice's queue. The cap trips on the ~200th
    // provisional (one internal setup event also counts toward the total).
    let mut sent = 0;
    loop {
        uas.respond(180, "Ringing").await;
        sent += 1;
        assert!(sent <= 220, "cap should have tripped by ~200 provisionals, not {sent}");
        call.expect(180).await;
        if b2bua.metrics().message_cap_terminated_total() == 1 {
            break;
        }
    }
    assert!(sent >= 199, "the cap must defend only against a genuine storm (tripped at {sent})");

    // The still-unanswered a-leg gets the 503 cap cause as its final response (the
    // router cap path replies before begin-termination — see router.rs), and the
    // still-ringing b-leg gets a CANCEL.
    let final_resp = call.expect(503).await;
    assert_eq!(final_resp.status, 503, "a-leg INVITE resolves with the 503 cap cause");
    let mut cancel = bob.receive("CANCEL").await;
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;

    settle_until(|| store.stats().current_total == 0).await;
    assert_eq!(store.stats().current_total, 0, "limiter hold released at the cap teardown");
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// **Reliable-provisional (PRACK) loop hits the cap before connect.** The harder
/// cousin of `provisional_storm`: bob sends reliable `183(100rel,RSeq)`, alice
/// PRACKs each, bob 200s the PRACK — so every round is THREE counted events
/// (relayed 183 + relayed PRACK + relayed 200) and an in-flight PRACK
/// transaction is open at the instant the cap trips. This exercises the cap
/// teardown against a pending non-INVITE transaction (not just a bare 18x). The
/// invariant is unchanged: the cap fires, the a-leg gets the 503 cap cause, the
/// b-leg is CANCELled, the limiter releases, the call fully reaps, and the RFC
/// gate stays clean (no stranded PRACK / RSeq-RAck violation).
#[tokio::test(start_paused = true)]
async fn prack_loop_storm_before_connect_trips_the_cap_and_releases_the_limiter() {
    let h = Harness::new("b2bua-limit-prack-loop");
    let alice = h.agent("alice", "127.0.0.1:5066").await;
    let bob = h.agent("bob", "127.0.0.1:5076").await;

    let http = SimulatedHttpNetwork::new();
    let (store, _limiter_srv) = serve_limiter(&http).await;
    let decision = route_limited("127.0.0.1", 5076, "trunk-A", 1, 3_600);
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter_client(&http))
        .tune(|c| c.reaper_enabled = false)
        .start(&h, "b2bua", "127.0.0.1:5086")
        .await;

    // alice INVITEs advertising reliable provisionals.
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    assert_eq!(store.stats().current_total, 1, "in-setup call holds one limiter slot");

    // Reliable-provisional + PRACK loop until the cap (default 200) trips. The
    // tripping message is still relayed (in-flight serviced), so each deliver
    // succeeds; check the metric AFTER every delivery and break before the next
    // expect would race the teardown.
    let mut rseq = 0u32;
    loop {
        rseq += 1;
        assert!(rseq <= 90, "cap should have tripped within ~67 PRACK rounds, not {rseq}");
        uas.respond(183, "Session Progress")
            .with_header("Require", "100rel")
            .with_header("RSeq", &rseq.to_string())
            .with_sdp(ANSWER)
            .await;
        call.expect(183).await;
        if b2bua.metrics().message_cap_terminated_total() == 1 {
            break; // the 183 tripped the cap
        }
        let mut prack = call
            .send_request(InDialogMethod::Prack)
            .with_rack(&format!("{rseq} 1 INVITE"))
            .send()
            .await;
        let mut b_prack = bob.receive("PRACK").await;
        b_prack.respond(200, "OK").await;
        if b2bua.metrics().message_cap_terminated_total() == 1 {
            break; // the relayed PRACK tripped the cap
        }
        prack.expect(200).await;
        if b2bua.metrics().message_cap_terminated_total() == 1 {
            break; // the relayed 200(PRACK) tripped the cap
        }
    }
    assert!(rseq >= 40, "the cap must defend only against a genuine storm (tripped at round {rseq})");

    // Teardown: a-leg gets the 503 cap cause (same router cap path as the plain
    // provisional storm), the still-ringing b-leg gets a CANCEL, and the
    // in-flight PRACK is resolved/abandoned. Assert the a-leg 503 explicitly,
    // then drain the rest (relayed PRACK 200 / CANCEL ordering) and force-resolve
    // past the 32 s TerminatingTimeout.
    let final_resp = call.expect(503).await;
    assert_eq!(final_resp.status, 503, "a-leg INVITE resolves with the 503 cap cause");
    h.advance(Duration::from_secs(1)).await;
    alice.drain().await;
    bob.drain().await;
    h.advance(Duration::from_secs(33)).await;

    settle_until(|| store.stats().current_total == 0).await;
    assert_eq!(store.stats().current_total, 0, "limiter released after the PRACK-loop cap teardown");
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// **Established call exceeds the in-dialog message cap.** A confirmed call that
/// pumps in-dialog traffic without end (glare loop, OPTIONS storm, a peer that
/// never stops) must be capped too. End-to-end OPTIONS is relayed both ways
/// (`relay-options` + `relay-non-invite-200`), so each round trip counts two
/// events at the B2BUA: 100 clean rounds reach exactly 200, the 101st request
/// trips the cap. Teardown BYEs both legs and releases the limiter hold.
#[tokio::test(start_paused = true)]
async fn in_dialog_message_storm_trips_the_cap_and_releases_the_limiter() {
    let h = Harness::new("b2bua-limit-in-dialog-cap");
    let alice = h.agent("alice", "127.0.0.1:5062").await;
    let bob = h.agent("bob", "127.0.0.1:5072").await;

    let http = SimulatedHttpNetwork::new();
    let (store, _limiter_srv) = serve_limiter(&http).await;
    let decision = route_limited("127.0.0.1", 5072, "trunk-A", 1, 3_600);
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter_client(&http))
        // Keepalive far out so the ONLY in-dialog events are our OPTIONS — the
        // count is then exactly 2 per round, deterministic.
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
        })
        .start(&h, "b2bua", "127.0.0.1:5082")
        .await;

    // ── establish ────────────────────────────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    assert_eq!(store.stats().current_total, 1, "established call holds one limiter slot");

    // Storm the established dialog with in-dialog OPTIONS until the cap trips.
    // Each completed round counts two events at the B2BUA: the relayed request
    // and the relayed 200. The cap (default 200) trips mid-round on the ~100th
    // round — on either the request (relayed to bob in-flight, then teardown) or
    // the relayed 200 (relayed to alice in-flight, then teardown), so check the
    // metric at both points.
    let mut rounds = 0;
    loop {
        rounds += 1;
        assert!(rounds <= 130, "cap should trip by ~100 OPTIONS rounds, not {rounds}");
        let mut txn = dialog.request(InDialogMethod::Options, None).await;
        // The relayed OPTIONS reaches bob even on the round that trips the cap.
        let mut b_opt = bob.receive("OPTIONS").await;
        b_opt.respond(200, "OK").await;
        if b2bua.metrics().message_cap_terminated_total() == 1 {
            break; // tripped on the request
        }
        txn.expect(200).await; // clean round: the 200 relays back to alice
        if b2bua.metrics().message_cap_terminated_total() == 1 {
            break; // tripped on the relayed 200
        }
    }
    assert!(rounds >= 95, "the cap must defend only against a genuine storm (tripped at round {rounds})");

    // Teardown BYEs both legs; tolerate a trailing relayed OPTIONS request in
    // flight from the round that tripped the cap.
    let mut a_bye = alice.receive_tolerating("BYE", &["OPTIONS"]).await;
    a_bye.respond(200, "OK").await;
    let mut b_bye = bob.receive_tolerating("BYE", &["OPTIONS"]).await;
    b_bye.respond(200, "OK").await;

    settle_until(|| store.stats().current_total == 0).await;
    assert_eq!(store.stats().current_total, 0, "limiter hold released at the cap teardown");
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}
