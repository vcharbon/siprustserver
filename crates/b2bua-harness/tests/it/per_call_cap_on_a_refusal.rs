//! A per-call cap (ADR-0057) crossed by a turn of a call that dials a route
//! with a failover: the turn of the callee's failure final, of his dead
//! hop's timeout, of his ringing or its timeout, or of the caller's CANCEL.
//! The turn is discarded with what it held: the failure's record, its
//! consult of the decision service, the failed leg's end. What the
//! transaction layer did of the event stands all the same: a final it
//! ACKed (RFC 3261 §17.1.1.3), a CANCEL it holds unsent for a dead hop
//! (§9.1), the 487 it answered a CANCELled INVITE with (§9.2). Where nothing
//! more is coming from the callee, the verdict's teardown answers the
//! caller, who holds a 100 Trying, the bare 503 at once (§16.7, ADR-0022);
//! where his INVITE still rings, it CANCELs him and answers her on his 487.
//! It dials no further leg. The call writes one CDR naming the cap and
//! releases every limiter hold.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua::config::{AgeCap, B2buaConfig, Cap};
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, CallTreatment, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::LimiterEntry;
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use scenario_harness::callflow::OFFER_SDP;
use scenario_harness::{Agent, ClientInvite, Inbound, ServerTxn, SIMULATED_TRANSIT_DELAY_MS};
use sip_message::header::HeaderName;
use sip_message::SipResponse;
use tokio::time::Instant;

/// The port carol, the failover target, listens on.
const CAROL_PORT: u16 = 5071;

/// The Call-ID of alice's INVITE, which names her call on the B2BUA.
const CALL_ID: &str = "per-call-cap-on-a-refusal@127.0.0.1";

/// The From tag of alice's INVITE.
const FROM_TAG: &str = "alice-tag";

/// The CDR events a call holds while its first route is dialled and has
/// answered nothing: the cap that holds it there, so the event recording
/// bob's refusal, his ringing or his INVITE's timeout crosses it.
const CDR_EVENTS_DIALLED: u64 = 2;

/// Fuel the turns before bob's refusal fit in, and the refusal's turn, which
/// records the failure, keeps its image, ends bob's leg and raises the
/// consult, does not. The verdict's teardown must fit in it too.
const ACTIONS_UNDER_A_REFUSAL: u64 = 3;

/// An ext value of one byte: the call writes none before the failure image
/// bob's refusal's turn keeps for the caller's final.
const EXT_BYTES_UNDER_A_FAILURE_IMAGE: u64 = 1;

/// The length of the `Warning` text the refusal of the record-size cell
/// carries, which its ring entry keeps byte for byte.
const LONG_TEXT: usize = 2048;

/// The longest alice waits for her final after the event that ended bob's
/// INVITE: one T1 (RFC 3261 §17.1.1.1), above the two hops of a 486 in and
/// the 503 out, and below every timer of the call.
const ANSWERED_WITHIN: Duration = Duration::from_millis(500);

/// How long the scene runs on past the trip before it looks for a late
/// failover leg: past the teardown's `TerminatingTimeout` (32 s), which
/// forces terminal a call the teardown left waiting.
const PAST_THE_TEARDOWN: Duration = Duration::from_secs(33);

/// One hop of the simulated network.
const HOP: Duration = Duration::from_millis(SIMULATED_TRANSIT_DELAY_MS);

/// The first-response deadline of an initial INVITE (ADR-0044), at its
/// default: an INVITE nothing answered by then has a dead hop.
const FIRST_RESPONSE_DEADLINE: Duration = Duration::from_secs(5);

/// The INVITE bound at its floor, with no setup deadline under it: past a
/// provisional, the transaction's give-up is what ends the ring.
const INVITE_BOUND_SEC: i64 = 33;

/// The CDR events a call holds while its first route rings: the cap that
/// holds it there, so the event recording the ring's timeout crosses it.
const CDR_EVENTS_RINGING: u64 = 3;

/// Routes to `bob_port` holding `trunk-a`, with a callback context, so a
/// failure consults the decision service; its answer fails over to carol,
/// holding `trunk-b`. Each consult answered counts on `consults`.
fn failover_routes(bob_port: u16, consults: Arc<AtomicUsize>) -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut r = route_to("127.0.0.1", bob_port);
                r.callback_context = Some("ctx".into());
                r.call_limiter = vec![LimiterEntry { id: "trunk-a".into(), limit: 10 }];
                NewCallResponse::Route(r)
            })
            .on_failure(move |_| {
                consults.fetch_add(1, Ordering::SeqCst);
                let mut r = route_to("127.0.0.1", CAROL_PORT);
                r.call_limiter = vec![LimiterEntry { id: "trunk-b".into(), limit: 10 }];
                CallTreatment::Route(r)
            })
            .build(),
    )
}

/// alice, bob and the B2BUA under `tune`, routing over [`failover_routes`],
/// with carol bound at [`CAROL_PORT`].
struct FailoverScene {
    s: B2buaScene,
    carol: Agent,
    consults: Arc<AtomicUsize>,
}

impl FailoverScene {
    async fn start(name: &str, tune: impl FnOnce(&mut B2buaConfig) + 'static) -> Self {
        let consults = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&consults);
        let s = B2buaScene::with_b2bua(name, move |bob_port| {
            B2buaSut::builder(failover_routes(bob_port, counted)).tune(tune)
        })
        .await;
        let carol = s.h.agent("carol", &format!("127.0.0.1:{CAROL_PORT}")).await;
        Self { s, carol, consults }
    }

    /// The failure consults the decision service answered so far.
    fn consults(&self) -> usize {
        self.consults.load(Ordering::SeqCst)
    }

    /// Each cap that tripped so far, with its count.
    fn trips(&self) -> Vec<(Cap, u64)> {
        let m = self.s.b2bua.metrics();
        Cap::ALL.iter().map(|c| (*c, m.cap_trips_of(*c))).filter(|(_, n)| *n > 0).collect()
    }

    /// alice's INVITE, under [`CALL_ID`] and [`FROM_TAG`], relayed to bob:
    /// her call, and bob's transaction.
    async fn dial(&self) -> (ClientInvite, ServerTxn) {
        let s = &self.s;
        let invite = s.alice.invite(&s.bob).identity(CALL_ID, FROM_TAG).with_sdp(OFFER_SDP);
        let call = invite.through(s.b2bua.addr).send().await;
        (call, s.bob.receive("INVITE").await)
    }

    /// The B2BUA's live copy of alice's call, the image its next turn
    /// starts from, if it serves the call.
    fn live_call(&self) -> Option<call::Call> {
        self.s.b2bua.live_call(&call::derive_call_ref("w0", CALL_ID, FROM_TAG))
    }

    /// What reached bob since he was last read: a request's method or a
    /// response's status, `None` for nothing.
    async fn bob_drew(&self) -> Option<String> {
        self.s.bob.take_queued_inbound().await.map(|inbound| match inbound {
            Inbound::Request(txn) => txn.request().method().to_string(),
            Inbound::Response(resp) => resp.status().to_string(),
        })
    }

    /// The status of each final response queued to alice, her queue drained.
    async fn finals_queued_to_alice(&self) -> Vec<u16> {
        let mut finals = Vec::new();
        while let Some(inbound) = self.s.alice.take_queued_inbound().await {
            if let Inbound::Response(resp) = inbound {
                if resp.status() >= 200 {
                    finals.push(resp.status());
                }
            }
        }
        finals
    }
}

/// How bob refuses his INVITE: 486, after a 180 when he `rings`, stating
/// `warning` when given.
#[derive(Clone, Copy, Default)]
struct Refusal<'a> {
    rings: bool,
    warning: Option<&'a str>,
}

impl Refusal<'_> {
    /// bob refuses `uas`, alice's `call` reading his 180 when he rings: the
    /// instant of his 486.
    async fn send(self, call: &mut ClientInvite, uas: &mut ServerTxn) -> Instant {
        if self.rings {
            uas.respond(180, "Ringing").await;
            call.expect(180).await;
        }
        let refused = Instant::now();
        let busy = uas.respond(486, "Busy Here");
        match self.warning {
            Some(text) => busy.with_header("Warning", text).await,
            None => busy.await,
        }
        refused
    }
}

/// The stack's own 503: nothing says why.
fn assert_bare_503(resp: &SipResponse) {
    assert_eq!(resp.status(), 503);
    assert!(!resp.has(&HeaderName::Reason), "no Reason: {resp:?}");
    assert!(!resp.has(&HeaderName::Warning), "no Warning: {resp:?}");
}

/// The status of the non-2xx final `leg` retains for the INVITE it received,
/// `None` when it retains none.
fn retained_rejection_status(leg: &call::Leg) -> Option<u16> {
    let (datagram, _) = leg.invite_rejection()?.wire();
    let status_line = std::str::from_utf8(datagram).ok()?.lines().next()?;
    status_line.split(' ').nth(1)?.parse().ok()
}

/// The turn that ended bob's INVITE tripped `cap`, and nothing else did:
/// the trip is counted before any failure consult was answered, so the turn
/// that crossed the cap is that one.
fn assert_tripped_on_bob_s_end(f: &FailoverScene, cap: Cap) {
    assert_eq!(f.trips(), vec![(cap, 1)], "the turn ending bob's INVITE tripped {cap:?} alone");
    assert_eq!(f.consults(), 0, "the trip came before any failure consult");
}

/// bob refuses alice's call as `refusal` states, on a route that fails over
/// to carol, and the turn handling his 486 crosses `cap`. alice is answered
/// the bare 503 within [`ANSWERED_WITHIN`]; carol is never dialled, however
/// long the scene runs on; bob draws nothing past the ACK of his 486, a
/// CANCEL least of all (RFC 3261 §9.1); the call ends with one CDR naming
/// `cap`, every limiter hold released.
async fn a_refusal_past(f: FailoverScene, cap: Cap, refusal: Refusal<'_>) {
    let (mut call, mut uas) = f.dial().await;
    let refused = refusal.send(&mut call, &mut uas).await;
    f.s.bob.receive("ACK").await;
    assert_tripped_on_bob_s_end(&f, cap);

    let answered = call.try_expect_final(503).await;
    let waited = refused.elapsed();
    f.s.h.advance(PAST_THE_TEARDOWN).await;
    let carol_dialled = f.carol.try_receive_tolerating("INVITE", &[]).await.is_some();
    let bob_drew = f.bob_drew().await;
    let facts = format!(
        "alice's final {:?} after {waited:?}; carol dialled: {carol_dialled}; bob drew: \
         {bob_drew:?}; consults: {}; CDRs: {}",
        answered.as_ref().map(|(r, _)| r.status()),
        f.consults(),
        f.s.b2bua.cdr_records().len(),
    );
    let (resp, _) = answered.unwrap_or_else(|e| panic!("alice is answered 503 ({e}): {facts}"));
    assert_bare_503(&resp);
    assert!(waited <= ANSWERED_WITHIN, "alice's 503 is at once: {facts}");
    assert!(!carol_dialled, "carol is never dialled: {facts}");
    assert_eq!(bob_drew, None, "bob's leg ended with his 486: {facts}");
    assert_ended_by(&f.s, cap).await;
    let _ = f.s.finish().await;
}

/// The call is reaped with one CDR naming `cap`, which tripped once, and no
/// discharge forced the call terminal.
async fn assert_ended_by(s: &B2buaScene, cap: Cap) {
    settle_until(|| s.b2bua.is_reaped()).await;
    let cdrs = s.b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR: {cdrs:?}");
    let reasons: Vec<_> = cdrs[0].events.iter().filter_map(|e| e.reason.as_deref()).collect();
    assert!(reasons.contains(&cap.cdr_reason()), "the CDR names the cap: {reasons:?}");
    let m = s.b2bua.metrics();
    assert_eq!(m.cap_trips_of(cap), 1, "{cap:?} tripped once");
    assert_eq!(m.reaper_discharged_total(), 0, "the verdict's teardown ended the call");
}

/// A count cap a peer reaches: the CDR event recording bob's refusal.
#[tokio::test(start_paused = true)]
async fn a_refusal_past_the_cdr_events_cap_answers_the_caller_a_bare_503() {
    let f = FailoverScene::start("cap-refusal-cdr-events", |c| {
        c.caps.cdr_events = AgeCap { base: CDR_EVENTS_DIALLED, per_hour: 0 }
    })
    .await;
    a_refusal_past(f, Cap::CdrEvents, Refusal::default()).await;
}

/// The fuel, a defect's cap: the actions the refusal's turn returns.
#[tokio::test(start_paused = true)]
async fn a_refusal_past_the_actions_cap_answers_the_caller_a_bare_503() {
    let f = FailoverScene::start("cap-refusal-actions", |c| {
        c.caps.actions_per_turn = ACTIONS_UNDER_A_REFUSAL
    })
    .await;
    a_refusal_past(f, Cap::ActionsPerTurn, Refusal::default()).await;
}

/// A service's ext value, a defect's cap: the failure image the refusal's
/// turn keeps.
#[tokio::test(start_paused = true)]
async fn a_refusal_past_the_ext_bytes_cap_answers_the_caller_a_bare_503() {
    let f = FailoverScene::start("cap-refusal-ext-bytes", |c| {
        c.caps.ext_bytes = EXT_BYTES_UNDER_A_FAILURE_IMAGE
    })
    .await;
    a_refusal_past(f, Cap::ExtBytes, Refusal::default()).await;
}

/// bob rings before he refuses, so his leg is early when the refusal's turn
/// crosses the ext value's cap: the teardown still owes him no CANCEL, his
/// INVITE transaction being complete.
#[tokio::test(start_paused = true)]
async fn a_refusal_after_ringing_past_a_cap_cancels_nothing() {
    let f = FailoverScene::start("cap-refusal-after-ringing", |c| {
        c.caps.ext_bytes = EXT_BYTES_UNDER_A_FAILURE_IMAGE
    })
    .await;
    a_refusal_past(f, Cap::ExtBytes, Refusal { rings: true, ..Refusal::default() }).await;
}

/// The B2BUA under the record-size cap of `record_bytes`, its calls keeping
/// their messages in the record, each entry with its `Warning` value.
fn ringed(record_bytes: u64) -> impl FnOnce(&mut B2buaConfig) + 'static {
    move |c| {
        c.cdr.message_ring = 32;
        c.cdr.captured_headers = vec!["Warning".to_string()];
        c.caps.record_bytes = record_bytes;
    }
}

/// A `Warning` (RFC 3261 §20.43) carrying a text of [`LONG_TEXT`] bytes.
fn long_warning() -> String {
    format!("399 bob.example \"{}\"", "x".repeat(LONG_TEXT))
}

/// The record size a peer reaches: bob's refusal carries a long `Warning`,
/// which its ring entry keeps, and the cap sits half that text above the
/// largest record written before it, measured on a first call that alice
/// CANCELs once bob answered 100 (RFC 3261 §9.1).
#[tokio::test(start_paused = true)]
async fn a_refusal_past_the_record_size_cap_answers_the_caller_a_bare_503() {
    const NAME: &str = "cap-refusal-record-bytes";
    let before = {
        let f = FailoverScene::start(NAME, ringed(1 << 30)).await;
        let (mut call, mut uas) = f.dial().await;
        let before = f.s.b2bua.metrics().cap_peak_of(Cap::RecordBytes);
        uas.respond(100, "Trying").await;
        call.cancel().await.expect(200).await;
        f.s.bob.receive("CANCEL").await.respond(200, "OK").await;
        uas.respond(487, "Request Terminated").await;
        f.s.bob.receive("ACK").await;
        call.expect(487).await;
        let _ = f.s.finish().await;
        before
    };
    let cap = before + u64::try_from(LONG_TEXT / 2).expect("a length");
    let f = FailoverScene::start(NAME, ringed(cap)).await;
    let warning = long_warning();
    a_refusal_past(f, Cap::RecordBytes, Refusal { warning: Some(&warning), ..Refusal::default() })
        .await;
}

/// bob refuses alice's call 486 past the CDR events' cap, and alice CANCELs
/// her INVITE half a hop later, so her CANCEL reaches the B2BUA after the
/// refusal's turn and its verdict's teardown, which answered her 503 at
/// once. Her CANCEL is answered 200 all the same (RFC 3261 §9.2) and her
/// INVITE draws that one final (§17.2.1, held by the RFC audit); carol is
/// never dialled; the call ends with one CDR naming the cap.
#[tokio::test(start_paused = true)]
async fn a_cancel_after_a_refusal_s_trip_is_answered_200() {
    let f = FailoverScene::start("cap-refusal-cancel-after", |c| {
        c.caps.cdr_events = AgeCap { base: CDR_EVENTS_DIALLED, per_hour: 0 }
    })
    .await;
    let (mut call, mut uas) = f.dial().await;
    Refusal::default().send(&mut call, &mut uas).await;
    f.s.h.advance(HOP / 2).await;
    let mut cancel = call.cancel().await;
    f.s.bob.receive("ACK").await;
    assert_tripped_on_bob_s_end(&f, Cap::CdrEvents);

    let answered = call.try_expect_final(503).await;
    let cancel_answered = cancel.try_expect(200).await;
    f.s.h.advance(PAST_THE_TEARDOWN).await;
    let carol_dialled = f.carol.try_receive_tolerating("INVITE", &[]).await.is_some();
    let facts = format!(
        "alice's final {:?}; her CANCEL's {:?}; carol dialled: {carol_dialled}",
        answered.as_ref().map(|(r, _)| r.status()),
        cancel_answered.as_ref().map(SipResponse::status),
    );
    let (resp, _) = answered.unwrap_or_else(|e| panic!("alice is answered 503 ({e}): {facts}"));
    assert_bare_503(&resp);
    cancel_answered.unwrap_or_else(|e| panic!("alice's CANCEL is answered 200 ({e}): {facts}"));
    assert!(!carol_dialled, "carol is never dialled: {facts}");
    assert_ended_by(&f.s, Cap::CdrEvents).await;
    let _ = f.s.finish().await;
}

/// alice CANCELs her INVITE and bob refuses it 486 half a hop later, so her
/// CANCEL reaches the B2BUA first and its turn crosses the CDR events' cap.
/// The transaction layer answered her CANCEL 200 and her INVITE 487 (RFC
/// 3261 §9.2) whatever became of that turn: that 487 is her INVITE's one
/// final (§17.2.1, held by the RFC audit), the call retains it as her
/// INVITE's rejection, the one a takeover re-arms, and the CDR states no 503
/// to her. The teardown's CANCEL waits for bob's provisional, which his 486
/// overtakes, so bob draws nothing past the ACK of his 486; carol is never
/// dialled; the call ends with one CDR naming the cap.
#[tokio::test(start_paused = true)]
async fn a_cancel_before_a_refusal_past_a_cap_draws_one_final() {
    let f = FailoverScene::start("cap-refusal-cancel-before", |c| {
        c.caps.cdr_events = AgeCap { base: CDR_EVENTS_DIALLED, per_hour: 0 }
    })
    .await;
    let (mut call, mut uas) = f.dial().await;
    let mut cancel = call.cancel().await;
    f.s.h.advance(HOP / 2).await;
    uas.respond(486, "Busy Here").await;
    f.s.h.advance(HOP / 2 + HOP / 10).await;
    let trips_on_the_cancel = f.trips();
    let retained = f.live_call().map(|c| retained_rejection_status(&c.a_leg));
    f.s.bob.receive("ACK").await;

    let cancel_answered = cancel.try_expect(200).await;
    let answered = call.try_expect_final(487).await;
    f.s.h.advance(PAST_THE_TEARDOWN).await;
    let carol_dialled = f.carol.try_receive_tolerating("INVITE", &[]).await.is_some();
    let bob_drew = f.bob_drew().await;
    settle_until(|| f.s.b2bua.is_reaped()).await;
    let cdrs = f.s.b2bua.cdr_records();
    let to_alice: Vec<_> = cdrs
        .iter()
        .flat_map(|r| r.events.iter())
        .filter(|e| e.leg_id == "a")
        .filter_map(|e| e.status_code)
        .collect();
    let facts = format!(
        "trips on the CANCEL {trips_on_the_cancel:?}; consults {}; the rejection the call \
         retains for alice {retained:?}; alice's final {:?}; her CANCEL's {:?}; carol dialled: \
         {carol_dialled}; bob drew {bob_drew:?}; the CDR's statuses to alice {to_alice:?}",
        f.consults(),
        answered.as_ref().map(|(r, _)| r.status()),
        cancel_answered.as_ref().map(SipResponse::status),
    );
    let cancel_trip = vec![(Cap::CdrEvents, 1)];
    assert_eq!(trips_on_the_cancel, cancel_trip, "the CANCEL's turn tripped: {facts}");
    assert_eq!(retained, Some(Some(487)), "the call retains alice's 487: {facts}");
    cancel_answered.unwrap_or_else(|e| panic!("alice's CANCEL is answered 200 ({e}): {facts}"));
    answered.unwrap_or_else(|e| panic!("alice's INVITE is answered 487 ({e}): {facts}"));
    assert!(!to_alice.contains(&503), "the CDR states no 503 to alice: {facts}");
    assert!(!carol_dialled, "carol is never dialled: {facts}");
    assert_eq!(bob_drew, None, "bob's leg ended with his 486: {facts}");
    assert_ended_by(&f.s, Cap::CdrEvents).await;
    let _ = f.s.finish().await;
}

/// The cap trips on the turn of bob's 180 instead: that turn is discarded,
/// the 180 with it, so alice hears no ringing, and bob's INVITE still rings,
/// so the teardown CANCELs it (RFC 3261 §9.1). bob answers the CANCEL 200
/// and his INVITE 487, which the B2BUA ACKs, and alice is answered the bare
/// 503 then; carol is never dialled; one CDR names the cap.
#[tokio::test(start_paused = true)]
async fn a_trip_on_the_callee_s_ringing_cancels_him() {
    let f = FailoverScene::start("cap-ringing", |c| {
        c.caps.cdr_events = AgeCap { base: CDR_EVENTS_DIALLED, per_hour: 0 }
    })
    .await;
    let (mut call, mut uas) = f.dial().await;
    uas.respond(180, "Ringing").await;
    f.s.h.advance(HOP).await;
    let trips = f.trips();
    let cancel = f.s.bob.try_receive("CANCEL").await;
    let facts = format!(
        "trips {trips:?}; consults {}; bob CANCELled: {:?}",
        f.consults(),
        cancel.as_ref().map(|_| ()),
    );
    assert_eq!(trips, vec![(Cap::CdrEvents, 1)], "the 180's turn tripped the cap: {facts}");
    let mut cancel =
        cancel.unwrap_or_else(|e| panic!("bob is CANCELled, his INVITE ringing ({e}): {facts}"));
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    f.s.bob.receive("ACK").await;

    let (resp, ringing) = call.try_expect_final(503).await.expect("alice is answered 503");
    assert_bare_503(&resp);
    let ringing: Vec<_> = ringing.iter().map(SipResponse::status).collect();
    assert_eq!(ringing, Vec::<u16>::new(), "alice hears no ringing");
    f.s.h.advance(PAST_THE_TEARDOWN).await;
    let carol_dialled = f.carol.try_receive_tolerating("INVITE", &[]).await.is_some();
    assert!(!carol_dialled, "carol is never dialled");
    assert_ended_by(&f.s, Cap::CdrEvents).await;
    let _ = f.s.finish().await;
}

/// bob answers nothing, not even a 100, and the turn of the first-response
/// deadline's timeout (ADR-0044) crosses `cap`. His hop is dead: the
/// teardown's CANCEL waits unsent in his INVITE transaction for a
/// provisional that never comes (RFC 3261 §9.1), so the call waits on
/// nothing from him and alice is answered the bare 503 at once; carol is
/// never dialled; bob draws nothing; one CDR names the cap.
async fn a_dead_hop_past(f: FailoverScene, cap: Cap) {
    let (mut call, _dead) = f.dial().await;
    let deadline = Instant::now() + FIRST_RESPONSE_DEADLINE;
    f.s.h.advance(FIRST_RESPONSE_DEADLINE).await;
    let trips = f.trips();

    let answered = call.try_expect_final(503).await;
    let waited = deadline.elapsed();
    f.s.h.advance(PAST_THE_TEARDOWN).await;
    let carol_dialled = f.carol.try_receive_tolerating("INVITE", &[]).await.is_some();
    let bob_drew = f.bob_drew().await;
    let facts = format!(
        "trips {trips:?}; consults {}; alice's final {:?} {waited:?} past the deadline; carol \
         dialled: {carol_dialled}; bob drew {bob_drew:?}",
        f.consults(),
        answered.as_ref().map(|(r, _)| r.status()),
    );
    assert_eq!(trips, vec![(cap, 1)], "the timeout's turn tripped {cap:?}: {facts}");
    assert_eq!(f.consults(), 0, "the trip came before any failure consult: {facts}");
    let (resp, _) = answered.unwrap_or_else(|e| panic!("alice is answered 503 ({e}): {facts}"));
    assert_bare_503(&resp);
    assert!(waited <= ANSWERED_WITHIN, "alice's 503 is at once: {facts}");
    assert!(!carol_dialled, "carol is never dialled: {facts}");
    assert_eq!(bob_drew, None, "bob, who answered nothing, is sent nothing: {facts}");
    assert_ended_by(&f.s, cap).await;
    let _ = f.s.finish().await;
}

/// A count cap a peer reaches: the CDR event recording bob's dead hop.
#[tokio::test(start_paused = true)]
async fn a_dead_hop_past_the_cdr_events_cap_answers_the_caller_a_bare_503() {
    let f = FailoverScene::start("cap-dead-hop-cdr-events", |c| {
        c.caps.cdr_events = AgeCap { base: CDR_EVENTS_DIALLED, per_hour: 0 }
    })
    .await;
    a_dead_hop_past(f, Cap::CdrEvents).await;
}

/// The fuel: the actions the dead hop's turn returns. The teardown, which
/// holds the CANCEL as well as the 503, fits in it.
#[tokio::test(start_paused = true)]
async fn a_dead_hop_past_the_actions_cap_answers_the_caller_a_bare_503() {
    let f = FailoverScene::start("cap-dead-hop-actions", |c| {
        c.caps.actions_per_turn = ACTIONS_UNDER_A_REFUSAL
    })
    .await;
    a_dead_hop_past(f, Cap::ActionsPerTurn).await;
}

/// bob rings, then goes silent past the INVITE bound, and the turn of his
/// transaction's timeout crosses the CDR events' cap. His INVITE still
/// rings: the teardown abandons it with a CANCEL (RFC 3261 §9.1), whose 487
/// is still coming, as a trip on any turn of a ringing call does, so alice
/// holds no final when bob's CANCEL arrives. bob answers the CANCEL 200 and
/// his INVITE 487, which the B2BUA ACKs (§17.1.1.3), and alice is answered
/// the bare 503 then; carol is never dialled; one CDR names the cap.
#[tokio::test(start_paused = true)]
async fn a_ringing_timeout_past_the_cdr_events_cap_cancels_the_callee() {
    let f = FailoverScene::start("cap-ringing-timeout", |c| {
        c.invite_txn_timeout_sec = INVITE_BOUND_SEC;
        c.setup_timeout_ms = 0;
        c.keepalive_interval_sec = 300;
        c.caps.cdr_events = AgeCap { base: CDR_EVENTS_RINGING, per_hour: 0 };
    })
    .await;
    let (mut call, mut uas) = f.dial().await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    f.s.h.advance(Duration::from_secs(INVITE_BOUND_SEC as u64)).await;
    let trips = f.trips();
    let cancel = f.s.bob.try_receive("CANCEL").await;
    let facts = format!(
        "trips {trips:?}; consults {}; bob CANCELled: {:?}",
        f.consults(),
        cancel.as_ref().map(|_| ()),
    );
    assert_eq!(trips, vec![(Cap::CdrEvents, 1)], "the timeout's turn tripped the cap: {facts}");
    assert_eq!(f.consults(), 0, "the trip came before any failure consult: {facts}");
    let mut cancel =
        cancel.unwrap_or_else(|e| panic!("bob is CANCELled, his INVITE ringing ({e}): {facts}"));
    let early = f.finals_queued_to_alice().await;
    assert_eq!(early, Vec::<u16>::new(), "alice holds no final before bob's 487: {facts}");
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    f.s.bob.try_receive("ACK").await.expect("the 487 the CANCEL provoked is ACKed");

    let (resp, _) = call.try_expect_final(503).await.expect("alice is answered 503");
    assert_bare_503(&resp);
    f.s.h.advance(PAST_THE_TEARDOWN).await;
    let carol_dialled = f.carol.try_receive_tolerating("INVITE", &[]).await.is_some();
    assert!(!carol_dialled, "carol is never dialled");
    assert_ended_by(&f.s, Cap::CdrEvents).await;
    let _ = f.s.finish().await;
}
