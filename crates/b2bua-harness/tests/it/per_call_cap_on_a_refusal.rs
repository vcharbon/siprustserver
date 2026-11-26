//! A per-call cap (ADR-0057) crossed by the turn that handles a callee's
//! failure final, on a route with a failover. The turn is discarded with
//! what it held: the failure's record, its consult of the decision service,
//! the failed leg's end. The callee's INVITE transaction is complete all the
//! same, ACKed by the transaction layer (RFC 3261 §17.1.1.3), so no final is
//! coming on that leg: the verdict's teardown answers the caller, who holds
//! a 100 Trying, the bare 503 at once (§16.7, ADR-0022), dials no further
//! leg and draws nothing more from the callee. The call writes one CDR
//! naming the cap and releases every limiter hold.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use b2bua::config::{AgeCap, B2buaConfig, Cap};
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, CallTreatment, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::LimiterEntry;
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use scenario_harness::callflow::OFFER_SDP;
use scenario_harness::{Agent, ClientInvite, ServerTxn, StepError, SIMULATED_TRANSIT_DELAY_MS};
use sip_message::header::HeaderName;
use sip_message::SipResponse;
use tokio::time::Instant;

/// The port carol, the failover target, listens on.
const CAROL_PORT: u16 = 5071;

/// The CDR events a call holds while its first route is dialled and has
/// answered nothing: the cap that holds it there, so the event recording
/// bob's refusal crosses it.
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

/// The longest alice waits for her final after bob's refusal: one T1 (RFC
/// 3261 §17.1.1.1), above the two hops of the 486 in and the 503 out, and
/// below every timer of the call.
const ANSWERED_WITHIN: Duration = Duration::from_millis(500);

/// How long the scene runs on past the trip before it looks for a late
/// failover leg: past the teardown's `TerminatingTimeout` (32 s), which
/// forces terminal a call the teardown left waiting.
const PAST_THE_TEARDOWN: Duration = Duration::from_secs(33);

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

    /// alice's INVITE, relayed to bob: her call, and bob's transaction.
    async fn dial(&self) -> (ClientInvite, ServerTxn) {
        let s = &self.s;
        let call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
        (call, s.bob.receive("INVITE").await)
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

/// The turn of bob's refusal tripped `cap`, and nothing else did: the trip
/// is counted by the time bob's ACK arrives, before any failure consult was
/// answered, so the turn that crossed the cap is the refusal's.
fn assert_tripped_on_the_refusal(f: &FailoverScene, cap: Cap) {
    let m = f.s.b2bua.metrics();
    let trips: Vec<_> = Cap::ALL.iter().map(|c| (*c, m.cap_trips_of(*c))).collect();
    let tripped: Vec<_> = trips.iter().filter(|(_, n)| *n > 0).collect();
    assert_eq!(tripped, vec![&(cap, 1)], "bob's refusal's turn tripped {cap:?} alone");
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
    assert_tripped_on_the_refusal(&f, cap);

    let answered = call.try_expect_final(503).await;
    let waited = refused.elapsed();
    f.s.h.advance(PAST_THE_TEARDOWN).await;
    let carol_dialled = f.carol.try_receive_tolerating("INVITE", &[]).await.is_some();
    let bob_drew = f.s.bob.try_receive_tolerating("CANCEL", &[]).await.is_some();
    let facts = format!(
        "alice's final {:?} after {waited:?}; carol dialled: {carol_dialled}; bob drew a CANCEL: \
         {bob_drew}; consults: {}; CDRs: {}",
        answered.as_ref().map(|(r, _)| r.status()),
        f.consults(),
        f.s.b2bua.cdr_records().len(),
    );
    let (resp, _) = answered.unwrap_or_else(|e| panic!("alice is answered 503 ({e}): {facts}"));
    assert_bare_503(&resp);
    assert!(waited <= ANSWERED_WITHIN, "alice's 503 is at once: {facts}");
    assert!(!carol_dialled, "carol is never dialled: {facts}");
    assert!(!bob_drew, "bob's leg ended with his 486: {facts}");
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

/// alice's final to her INVITE, 487 or 503.
async fn final_of(call: &mut ClientInvite) -> u16 {
    match call.try_expect_final(503).await {
        Ok((resp, _)) => {
            assert_bare_503(&resp);
            503
        }
        Err(StepError::WrongStatus { got: 487, .. }) => 487,
        Err(e) => panic!("alice is answered 487 or 503: {e}"),
    }
}

/// bob refuses alice's call 486 past the CDR events' cap, and alice CANCELs
/// her INVITE half a hop later, crossing whatever the B2BUA answers her.
/// Her CANCEL is answered 200 (RFC 3261 §9.2) and her INVITE draws one
/// final, the 487 or the 503, never both (§17.2.1, held by the RFC audit);
/// carol is never dialled; the call ends with one CDR naming the cap.
#[tokio::test(start_paused = true)]
async fn a_cancel_crossing_a_refusal_s_trip_draws_one_final() {
    let f = FailoverScene::start("cap-refusal-cancel-crossing", |c| {
        c.caps.cdr_events = AgeCap { base: CDR_EVENTS_DIALLED, per_hour: 0 }
    })
    .await;
    let (mut call, mut uas) = f.dial().await;
    Refusal::default().send(&mut call, &mut uas).await;
    f.s.h.advance(Duration::from_millis(SIMULATED_TRANSIT_DELAY_MS / 2)).await;
    let mut cancel = call.cancel().await;
    f.s.bob.receive("ACK").await;
    assert_tripped_on_the_refusal(&f, Cap::CdrEvents);

    let status = final_of(&mut call).await;
    cancel.expect(200).await;
    f.s.h.advance(PAST_THE_TEARDOWN).await;
    let carol_dialled = f.carol.try_receive_tolerating("INVITE", &[]).await.is_some();
    assert!(!carol_dialled, "carol is never dialled; alice was answered {status}");
    assert_ended_by(&f.s, Cap::CdrEvents).await;
    let _ = f.s.finish().await;
}
