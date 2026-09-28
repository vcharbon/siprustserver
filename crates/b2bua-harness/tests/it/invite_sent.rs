//! `InviteSent`: one CDR event per leg this stack originates, written when
//! the leg's INVITE is emitted and stamped with the decision it is dialed
//! under — the first route's leg, a failover's replacement leg and a
//! service's media leg alike — and none for a leg refused before its INVITE
//! is minted. The SUT keeps every terminated `Call` whole, so the test reads
//! its legs' kinds.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use b2bua::config::{B2buaConfig, CdrConfig};
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallLimiterEntry, CallTreatment, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::limiter::{
    AdmitOutcome, CallLimiter, LimiterEntry, RefreshAnswer, RefreshCall, ReleaseAnswer,
};
use b2bua::rules::ServiceDef;
use b2bua_harness::{settle_until, B2buaSut};
use call::{Call, CdrEventType, LegKind};
use scenario_harness::Harness;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
const MRF_SDP: &str = "v=0\r\no=mrf 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

const ALICE: &str = "127.0.0.1:5060";
const CAROL: &str = "127.0.0.1:5070";
const BOB: &str = "127.0.0.1:5071";
const MRF: &str = "127.0.0.1:5072";
const B2BUA: &str = "127.0.0.1:5080";

/// The SUT under `decision`, its own `limiter` if any, and `services`, every
/// target admitted, keeping every terminated `Call` whole.
struct Sut {
    addr: SocketAddr,
    b2bua: B2buaSut,
}

impl Sut {
    async fn spawn(
        h: &Harness,
        decision: Arc<dyn CallDecisionEngine>,
        limiter: Option<Arc<dyn CallLimiter>>,
        services: Vec<ServiceDef>,
    ) -> Self {
        Self::spawn_tuned(h, decision, limiter, services, |_| {}).await
    }

    async fn spawn_tuned(
        h: &Harness,
        decision: Arc<dyn CallDecisionEngine>,
        limiter: Option<Arc<dyn CallLimiter>>,
        services: Vec<ServiceDef>,
        tune: impl FnOnce(&mut B2buaConfig) + 'static,
    ) -> Self {
        let mut builder =
            B2buaSut::builder(decision).services(services).keep_terminated_calls().tune(|c| {
                c.worker_allowed_target_suffixes = vec!["*".into()];
                c.cdr = CdrConfig { message_ring: 32, captured_headers: Vec::new() };
                tune(c);
            });
        if let Some(limiter) = limiter {
            builder = builder.limiter(limiter);
        }
        let b2bua = builder.start(h, "b2bua", B2BUA).await;
        Self { addr: b2bua.addr, b2bua }
    }

    /// Every call created is reaped, every limiter hold released, and the
    /// one CDR is written; the record carries the events as the call holds
    /// them.
    async fn assert_reaped(&self) -> Call {
        settle_until(|| self.b2bua.is_reaped() && self.b2bua.cdr_records().len() == 1).await;
        self.b2bua.assert_fully_reaped();
        let terminated = self.b2bua.terminated_calls();
        assert_eq!(terminated.len(), 1, "exactly one CDR per call");
        let call = terminated.into_iter().next().unwrap();
        let records = self.b2bua.cdr_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].events, call.cdr_events);
        call
    }
}

/// `(leg, decision_ordinal)` of every `InviteSent`, in order.
fn invites_sent(call: &Call) -> Vec<(&str, u32)> {
    call.cdr_events
        .iter()
        .filter(|e| e.event_type == CdrEventType::InviteSent)
        .map(|e| (e.leg_id.as_str(), e.decision_ordinal))
        .collect()
}

fn leg<'a>(call: &'a Call, id: &str) -> &'a call::Leg {
    call.b_legs.iter().find(|l| l.leg_id == id).unwrap_or_else(|| panic!("leg {id}"))
}

/// A routed, answered and released call: the one leg holds one `InviteSent`,
/// under the route that dialed it, at the instant its INVITE left.
#[tokio::test(start_paused = true)]
async fn a_plain_call_holds_exactly_one_invite_sent() {
    let h = Harness::new("invite-sent-plain");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let decision = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5071));
    let sut = Sut::spawn(&h, decision, None, Vec::new()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let done = sut.assert_reaped().await;
    assert_eq!(invites_sent(&done), vec![("b-1", 1)]);
    let sent = done.cdr_events.iter().find(|e| e.event_type == CdrEventType::InviteSent).unwrap();
    assert_eq!(sent.status_code, None);
    assert_eq!(sent.reason, None);
    let invite = &leg(&done, "b-1").messages.entries[0];
    assert_eq!(sent.timestamp, invite.at_ms, "stamped at the turn the INVITE left in");
    assert_eq!(invite.decision_ordinal, 1, "the ring entry agrees with the event");

    let _report = h.finish().await;
}

/// A call whose first leg fails and a failover route replaces it: the first
/// leg's `InviteSent` under the route, the replacement's under the failover
/// route, one each.
#[tokio::test(start_paused = true)]
async fn a_failover_call_holds_one_invite_sent_per_leg() {
    let h = Harness::new("invite-sent-failover");
    let alice = h.agent("alice", ALICE).await;
    let carol = h.agent("carol", CAROL).await;
    let bob = h.agent("bob", BOB).await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("ctx".into());
                NewCallResponse::Route(r)
            })
            .on_failure(|_| CallTreatment::Route(route_to("127.0.0.1", 5071)))
            .build(),
    );
    let sut = Sut::spawn(&h, decision, None, Vec::new()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    carol.receive("INVITE").await.respond(486, "Busy Here").await;
    carol.receive("ACK").await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let done = sut.assert_reaped().await;
    assert_eq!(invites_sent(&done), vec![("b-1", 1), ("b-2", 2)]);
    assert_eq!(done.b_legs.len(), 2);
    for (id, ordinal) in [("b-1", 1), ("b-2", 2)] {
        let sent = done
            .cdr_events
            .iter()
            .find(|e| e.event_type == CdrEventType::InviteSent && e.leg_id == id)
            .unwrap();
        let invite = &leg(&done, id).messages.entries[0];
        assert_eq!(sent.timestamp, invite.at_ms, "{id} is stamped at the turn its INVITE left in");
        assert_eq!(invite.decision_ordinal, ordinal, "{id}'s ring entry agrees with the event");
    }

    let _report = h.finish().await;
}

/// A decision that defers routing to the `announcement` service, which dials
/// a media leg toward the MRF and the destination after the clip.
fn announcement_decision() -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5071);
                r.service_ext.insert(
                    "announcement".into(),
                    serde_json::json!({
                        "clip_id": "intro-001",
                        "mrf_host": "127.0.0.1",
                        "mrf_port": 5072,
                        "dest_host": "127.0.0.1",
                        "dest_port": 5071,
                        "defer_routing": true,
                    }),
                );
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// A leg a service originates is recorded like one the route dials: the
/// media leg toward the MRF and the destination leg dialed after the clip
/// each hold one `InviteSent`, both under the one route.
#[tokio::test(start_paused = true)]
async fn a_media_leg_holds_its_invite_sent() {
    let h = Harness::new("invite-sent-media-leg");
    let alice = h.agent("alice", ALICE).await;
    let mrf = h.agent("mrf", MRF).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h, announcement_decision(), None, vec![announcement::service()]).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut mrf_uas = mrf.receive("INVITE").await;
    mrf_uas.respond(200, "OK").with_sdp(MRF_SDP).await;
    mrf.receive("ACK").await;
    let mut mrf_dialog = mrf_uas.dialog();
    call.expect(183).await;
    mrf.receive("INFO").await.respond(200, "OK").await;
    let done_body = String::from_utf8(announcement::mscml::build_response(200)).unwrap();
    let mut clip_done = mrf_dialog
        .send_request(InDialogMethod::Info)
        .with_header("Content-Type", "application/mediaservercontrol+xml")
        .with_sdp(&done_body)
        .send()
        .await;
    clip_done.expect(200).await;
    mrf.receive("BYE").await.respond(200, "OK").await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let done = sut.assert_reaped().await;
    assert_eq!(leg(&done, "b-1").kind, Some(LegKind::Media), "the first leg is the media leg");
    assert_eq!(leg(&done, "b-2").kind, Some(LegKind::Destination), "the second is the destination");
    assert_eq!(invites_sent(&done), vec![("b-1", 1), ("b-2", 1)]);

    let _report = h.finish().await;
}

/// A limiter that refuses every admission naming `refused` and admits the
/// rest without holds.
struct RefusingLimiter(&'static str);

#[async_trait]
impl CallLimiter for RefusingLimiter {
    async fn admit(&self, _: &str, entries: &[LimiterEntry], _: bool) -> AdmitOutcome {
        match entries.iter().find(|e| e.id == self.0) {
            Some(e) => AdmitOutcome::Rejected { limiter_id: e.id.clone() },
            None => AdmitOutcome::Unavailable,
        }
    }
    async fn release(&self, _keys: &[String]) -> ReleaseAnswer {
        ReleaseAnswer::Released
    }
    async fn refresh(&self, _: &[RefreshCall]) -> RefreshAnswer {
        RefreshAnswer::Unavailable
    }
    fn report_to(&self, _: b2bua::limiter::LimiterReports) {}
}

/// A route the limiter refuses dials nothing and leaves no `InviteSent`; the
/// failover route the refusal raises dials the one leg, recorded once.
#[tokio::test(start_paused = true)]
async fn a_limiter_refused_route_holds_no_invite_sent() {
    let h = Harness::new("invite-sent-limiter-refused");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.callback_context = Some("ctx".into());
                r.call_limiter = vec![CallLimiterEntry { id: "cap".into(), limit: 1 }];
                NewCallResponse::Route(r)
            })
            .on_failure(|req| {
                assert_eq!(req.failure.origin, "call_limiter");
                CallTreatment::Route(route_to("127.0.0.1", 5071))
            })
            .build(),
    );
    let sut = Sut::spawn(&h, decision, Some(Arc::new(RefusingLimiter("cap"))), Vec::new()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let done = sut.assert_reaped().await;
    assert_eq!(done.b_legs.len(), 1, "the refused route dialed nothing");
    assert_eq!(invites_sent(&done), vec![("b-1", 1)]);

    let _report = h.finish().await;
}

/// A leg the target admission refuses in the executor is never minted: the
/// call ends on the `Reject` the refusal writes and holds no `InviteSent`.
#[tokio::test(start_paused = true)]
async fn an_admission_refused_leg_holds_no_invite_sent() {
    let h = Harness::new("invite-sent-admission-refused");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5071);
                r.service_ext.insert(
                    "announcement".into(),
                    serde_json::json!({
                        "clip_id": "intro-001",
                        "mrf_host": "mrf.example",
                        "mrf_port": 5072,
                        "dest_host": "127.0.0.1",
                        "dest_port": 5071,
                        "defer_routing": true,
                    }),
                );
                NewCallResponse::Route(r)
            })
            .build(),
    );
    // Only the production suffix is admitted: `mrf.example` is refused.
    let sut = Sut::spawn_tuned(&h, decision, None, vec![announcement::service()], |c| {
        c.worker_allowed_target_suffixes = vec![".svc.cluster.local".into()]
    })
    .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    call.expect(503).await;

    let done = sut.assert_reaped().await;
    assert!(done.b_legs.is_empty(), "the refused leg was never minted");
    assert_eq!(invites_sent(&done), Vec::<(&str, u32)>::new());
    assert!(
        done.cdr_events.iter().any(|e| e.event_type == CdrEventType::Reject
            && e.status_code == Some(503)
            && e.reason.as_deref() == Some("admission_reject host=mrf.example")),
        "{:?}",
        done.cdr_events
    );

    let _report = h.finish().await;
}
