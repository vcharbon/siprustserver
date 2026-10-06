//! The deployment's minted-final advertisement
//! (`B2buaConfig::minted_final_advertisement`): a failure final to the caller's
//! INVITE that restates no peer's final carries it; one that restates the
//! failing callee's final carries the callee's lines and nothing of it; a
//! decision's own statement of a header, set or removal, outranks it.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, CallTreatment, HeaderUpdate, NewCallRequest,
    NewCallResponse, RejectDecision, SipHeaderUpdates,
};
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use sip_message::generators::CapabilitySet;
use sip_message::header::{AcceptRange, HeaderName};
use sip_message::SipResponse;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5160";
const CAROL: &str = "127.0.0.1:5170";
const B2BUA: &str = "127.0.0.1:5180";

const OWN_ACCEPT: &str = "application/sdp, application/isup";
const CAROL_ALLOW: &str = "INVITE, ACK, BYE, CANCEL";

/// The set under test: an `Accept` on every minted INVITE final.
fn advertisement() -> CapabilitySet {
    CapabilitySet::stating(
        None,
        None,
        Some(vec![AcceptRange::new("application/sdp"), AcceptRange::new("application/isup")]),
    )
}

/// What a decision reject states of `Accept`.
#[derive(Clone, Copy)]
enum Stated {
    Nothing,
    /// `Accept: text/plain`.
    Line,
    /// A removal of the name.
    Removal,
}

/// What the engine answers at the initial INVITE.
#[derive(Clone, Copy)]
enum Initial {
    /// A decision reject of this code.
    Reject(u16, Stated),
    /// Route to Carol, ringing for at most `Some(s)`.
    Route(Option<i64>),
}

/// What the engine answers at the failure consult.
#[derive(Clone, Copy)]
enum Failure {
    /// A decision reject of this code.
    Reject(u16, Stated),
    /// Relay the failing callee's final.
    Relay,
}

struct Engine {
    initial: Initial,
    failure: Failure,
}

fn reject(code: u16, stated: Stated) -> RejectDecision {
    let update = match stated {
        Stated::Nothing => None,
        Stated::Line => Some(HeaderUpdate::line("text/plain")),
        Stated::Removal => Some(HeaderUpdate::Remove),
    };
    let update_headers: Option<SipHeaderUpdates> =
        update.map(|u| [("Accept".to_string(), u)].into_iter().collect());
    RejectDecision {
        reject_code: code,
        reject_reason: None,
        update_headers,
        service_ext: Default::default(),
        label: None,
    }
}

#[async_trait]
impl CallDecisionEngine for Engine {
    async fn new_call(&self, _req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        match self.initial {
            Initial::Reject(code, stated) => Ok(NewCallResponse::Reject(reject(code, stated))),
            Initial::Route(no_answer) => {
                let mut r = route_to("127.0.0.1", 5170);
                r.callback_context = Some("ctx".into());
                r.no_answer_timeout_sec = no_answer;
                Ok(NewCallResponse::Route(r))
            }
        }
    }
    async fn call_failure(
        &self,
        _req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        Ok(match self.failure {
            Failure::Reject(code, stated) => CallTreatment::Reject(reject(code, stated)),
            Failure::Relay => CallTreatment::Relay { label: None },
        })
    }
    async fn call_refer(
        &self,
        _req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        Ok(CallReferResponse::Reject { code: 501, reason: None, label: None })
    }
}

async fn sut(h: &Harness, engine: Engine) -> (SocketAddr, B2buaSut) {
    let b2bua = B2buaSut::builder(Arc::new(engine))
        .tune(|c| {
            c.worker_allowed_target_suffixes = vec!["*".into()];
            c.minted_final_advertisement = advertisement();
        })
        .start(h, "b2bua", B2BUA)
        .await;
    (b2bua.addr, b2bua)
}

fn lines(resp: &SipResponse, name: &str) -> Vec<String> {
    resp.raw(HeaderName::from(name)).map(str::to_string).collect()
}

async fn reaped(b2bua: &B2buaSut) {
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// A reject the decision states before any leg is dialled is the call refused
/// in the element's own name: it carries the table's `Accept` and nothing else
/// of an advertisement.
#[tokio::test(start_paused = true)]
async fn a_reject_before_any_leg_carries_the_tables_accept() {
    let h = Harness::new("advert-reject-before-any-leg");
    let alice = h.agent("alice", ALICE).await;
    let carol = h.agent("carol", CAROL).await;
    let (addr, b2bua) =
        sut(&h, Engine { initial: Initial::Reject(404, Stated::Nothing), failure: Failure::Relay })
            .await;
    let mut call = alice.invite(&carol).with_sdp(OFFER).through(addr).send().await;
    let fin = call.expect(404).await;
    assert_eq!(lines(&fin, "Accept"), [OWN_ACCEPT]);
    assert!(lines(&fin, "Allow").is_empty());
    assert!(lines(&fin, "Supported").is_empty());
    reaped(&b2bua).await;
    let _report = h.finish().await;
}

/// The decision's own `Accept` on its reject is the more specific statement.
#[tokio::test(start_paused = true)]
async fn a_decision_stated_accept_outranks_the_table() {
    let h = Harness::new("advert-reject-stated-accept");
    let alice = h.agent("alice", ALICE).await;
    let carol = h.agent("carol", CAROL).await;
    let (addr, b2bua) =
        sut(&h, Engine { initial: Initial::Reject(403, Stated::Line), failure: Failure::Relay })
            .await;
    let mut call = alice.invite(&carol).with_sdp(OFFER).through(addr).send().await;
    let fin = call.expect(403).await;
    assert_eq!(lines(&fin, "Accept"), ["text/plain"]);
    reaped(&b2bua).await;
    let _report = h.finish().await;
}

/// A reject answering the failure consult restates the failing callee's
/// final: Carol's `Allow` rides, the table's `Accept` does not.
#[tokio::test(start_paused = true)]
async fn a_reject_restating_the_callees_final_carries_its_lines_and_not_the_tables() {
    let h = Harness::new("advert-reject-restating");
    let alice = h.agent("alice", ALICE).await;
    let carol = h.agent("carol", CAROL).await;
    let (addr, b2bua) = sut(
        &h,
        Engine { initial: Initial::Route(None), failure: Failure::Reject(403, Stated::Nothing) },
    )
    .await;
    let mut call = alice.invite(&carol).with_sdp(OFFER).through(addr).send().await;
    let mut uas = carol.receive("INVITE").await;
    uas.respond(603, "Decline").with_header("Allow", CAROL_ALLOW).await;
    carol.receive("ACK").await;
    let fin = call.expect(403).await;
    assert_eq!(lines(&fin, "Allow"), [CAROL_ALLOW]);
    assert!(lines(&fin, "Accept").is_empty(), "the callee stated none: {fin:?}");
    reaped(&b2bua).await;
    let _report = h.finish().await;
}

/// A callee that never answers leaves no final to restate: the reject the
/// consult draws is the element's own and carries the table's `Accept`.
#[tokio::test(start_paused = true)]
async fn a_reject_after_a_ring_timeout_carries_the_tables_accept() {
    let h = Harness::new("advert-reject-after-timeout");
    let alice = h.agent("alice", ALICE).await;
    let carol = h.agent("carol", CAROL).await;
    let (addr, b2bua) = sut(
        &h,
        Engine { initial: Initial::Route(Some(5)), failure: Failure::Reject(480, Stated::Nothing) },
    )
    .await;
    let mut call = alice.invite(&carol).with_sdp(OFFER).through(addr).send().await;
    let mut uas = carol.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    let mut cancel = carol.receive("CANCEL").await;
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    carol.receive("ACK").await;
    let fin = call.expect(480).await;
    assert_eq!(lines(&fin, "Accept"), [OWN_ACCEPT]);
    reaped(&b2bua).await;
    let _report = h.finish().await;
}

/// A decision removing `Accept` from its reject leaves the final without one,
/// before any leg and after a ring timeout alike.
#[tokio::test(start_paused = true)]
async fn a_decision_removal_of_accept_outranks_the_table() {
    let h = Harness::new("advert-reject-removes-accept");
    let alice = h.agent("alice", ALICE).await;
    let carol = h.agent("carol", CAROL).await;
    let engine = Engine { initial: Initial::Reject(404, Stated::Removal), failure: Failure::Relay };
    let (addr, b2bua) = sut(&h, engine).await;
    let mut call = alice.invite(&carol).with_sdp(OFFER).through(addr).send().await;
    let fin = call.expect(404).await;
    assert!(lines(&fin, "Accept").is_empty(), "{fin:?}");
    reaped(&b2bua).await;
    let _report = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_consult_reject_removing_accept_after_a_timeout_states_none() {
    let h = Harness::new("advert-consult-removes-accept");
    let alice = h.agent("alice", ALICE).await;
    let carol = h.agent("carol", CAROL).await;
    let engine =
        Engine { initial: Initial::Route(Some(5)), failure: Failure::Reject(480, Stated::Removal) };
    let (addr, b2bua) = sut(&h, engine).await;
    let mut call = alice.invite(&carol).with_sdp(OFFER).through(addr).send().await;
    let mut uas = carol.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    let mut cancel = carol.receive("CANCEL").await;
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    carol.receive("ACK").await;
    let fin = call.expect(480).await;
    assert!(lines(&fin, "Accept").is_empty(), "{fin:?}");
    reaped(&b2bua).await;
    let _report = h.finish().await;
}

/// A callee final with no relayable line is still a peer final: the reject
/// that restates it, and the relayed final, state nothing of the table's.
#[tokio::test(start_paused = true)]
async fn a_bare_callee_final_is_restated_without_the_tables_accept() {
    for (name, failure, status) in [
        ("advert-bare-reject", Failure::Reject(403, Stated::Nothing), 403),
        ("advert-bare-relay", Failure::Relay, 486),
    ] {
        let h = Harness::new(name);
        let alice = h.agent("alice", ALICE).await;
        let carol = h.agent("carol", CAROL).await;
        let (addr, b2bua) = sut(&h, Engine { initial: Initial::Route(None), failure }).await;
        let mut call = alice.invite(&carol).with_sdp(OFFER).through(addr).send().await;
        let mut uas = carol.receive("INVITE").await;
        uas.respond(486, "Busy Here").await;
        carol.receive("ACK").await;
        let fin = call.expect(status).await;
        assert!(lines(&fin, "Accept").is_empty(), "{name}: {fin:?}");
        reaped(&b2bua).await;
        let _report = h.finish().await;
    }
}

/// The admission ladder's 503 is a final minted in the worker's own name: the
/// core states its deployment set on the refusals it serves under, so a new
/// INVITE shed at the per-call queue cap carries it.
#[tokio::test(start_paused = true)]
async fn the_admission_ladders_503_carries_the_deployment_set() {
    let s = b2bua_harness::B2buaScene::with_b2bua("advert-admission-503", |bob_port| {
        B2buaSut::route_all_to("127.0.0.1", bob_port).tune(|c| {
            c.per_call_queue_cap = 1;
            c.minted_final_advertisement = advertisement();
        })
    })
    .await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let mut holding = s.establish().await;
    settle_until(|| s.b2bua.active_calls() == 1).await;
    let mut shed = carol.invite(&s.bob).with_sdp(OFFER).through(s.b2bua.addr).send().await;
    let refused = shed.expect(503).await;
    assert_eq!(lines(&refused, "Accept"), [OWN_ACCEPT]);
    s.hangup(&mut holding).await;
    settle_until(|| s.b2bua.cdr_records().len() == 1).await;
    settle_until(|| s.b2bua.is_reaped()).await;
    s.b2bua.assert_fully_reaped();
    let _ = s.finish().await;
}
