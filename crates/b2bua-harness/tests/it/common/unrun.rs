//! Fixtures for the scenarios where the B2BUA takes a message off the
//! transaction layer and its handler body does not run to an answer: a
//! decision engine that parks every call after the first (so one handler
//! permit stays held), and the dialog identity a test needs to write an
//! in-dialog request by hand and retransmit the very same datagram
//! (RFC 3261 §17.1.2.2).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse, ScriptedDecisionEngine,
};
use scenario_harness::callflow::{ANSWER_SDP, OFFER_SDP};
use scenario_harness::{Agent, Dialog};
use sip_message::SipResponse;

/// Routes the first call to one destination; every later `new_call` never
/// resolves, so its INVITE body holds a handler permit until the decision
/// deadline.
pub struct RouteFirstThenHang {
    inner: ScriptedDecisionEngine,
    calls: AtomicUsize,
}

impl RouteFirstThenHang {
    pub fn to(host: &str, port: u16) -> Self {
        Self { inner: ScriptedDecisionEngine::route_all_to(host, port), calls: AtomicUsize::new(0) }
    }
}

#[async_trait]
impl CallDecisionEngine for RouteFirstThenHang {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.inner.new_call(req).await
        } else {
            std::future::pending().await
        }
    }
    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        self.inner.call_failure(req).await
    }
    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.inner.call_refer(req).await
    }
}

/// What the caller side of a confirmed dialog needs to write an in-dialog
/// request by hand, so a test can retransmit the very same datagram.
pub struct DialogIds {
    call_id: String,
    from_uri: String,
    from_tag: String,
    to_uri: String,
    to_tag: String,
    /// The B2BUA's Contact: the remote target, carrying its `callRef`.
    remote_target: String,
}

impl DialogIds {
    pub fn of(answer: &SipResponse) -> Self {
        DialogIds {
            call_id: answer.call_id().as_str().to_string(),
            from_uri: answer.from().uri().to_string(),
            from_tag: answer.from().tag().expect("the INVITE carried a From-tag").to_string(),
            to_uri: answer.to().uri().to_string(),
            to_tag: answer.to().tag().expect("the 2xx carries a To-tag").to_string(),
            remote_target: answer
                .contacts()
                .as_slice()
                .first()
                .map(|c| c.uri().to_string())
                .expect("the 2xx carries a Contact"),
        }
    }

    /// The BYE `from` sends in this dialog, as one datagram.
    pub fn bye(&self, from: &Agent, cseq: u32, branch: &str) -> Vec<u8> {
        self.request(from, "BYE", cseq, branch, "")
    }

    /// An INFO `from` sends in this dialog, as one datagram.
    pub fn info(&self, from: &Agent, cseq: u32, branch: &str) -> Vec<u8> {
        self.request(from, "INFO", cseq, branch, "")
    }

    /// An offerless re-INVITE `from` sends in this dialog, as one datagram.
    pub fn reinvite(&self, from: &Agent, cseq: u32, branch: &str) -> Vec<u8> {
        let contact = format!("Contact: <sip:{}>\r\n", from.addr());
        self.request(from, "INVITE", cseq, branch, &contact)
    }

    /// The CANCEL of the re-INVITE sent on `branch` at `cseq` (RFC 3261 §9.1):
    /// same branch and CSeq number, method CANCEL.
    pub fn cancel(&self, from: &Agent, cseq: u32, branch: &str) -> Vec<u8> {
        self.request(from, "CANCEL", cseq, branch, "")
    }

    /// The hop-by-hop ACK to a non-2xx final for the re-INVITE sent on
    /// `branch` at `cseq` (RFC 3261 §17.1.1.3): same branch, the final's
    /// To-tag, which in a dialog is the dialog's own.
    pub fn ack_non_2xx(&self, from: &Agent, cseq: u32, branch: &str) -> Vec<u8> {
        self.request(from, "ACK", cseq, branch, "")
    }

    fn request(&self, from: &Agent, method: &str, cseq: u32, branch: &str, extra: &str) -> Vec<u8> {
        format!(
            "{method} {ruri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {via};branch=z9hG4bK-{branch}\r\n\
             Max-Forwards: 70\r\n\
             From: <{from_uri}>;tag={from_tag}\r\n\
             To: <{to_uri}>;tag={to_tag}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} {method}\r\n\
             {extra}\
             Content-Length: 0\r\n\r\n",
            ruri = self.remote_target,
            via = from.addr(),
            from_uri = self.from_uri,
            from_tag = self.from_tag,
            to_uri = self.to_uri,
            to_tag = self.to_tag,
            call_id = self.call_id,
        )
        .into_bytes()
    }
}

/// Set up a call by hand and keep the caller's 2xx.
pub async fn establish_keeping_answer(
    caller: &Agent,
    callee: &Agent,
    b2bua: SocketAddr,
) -> (Dialog, SipResponse) {
    let mut call = caller.invite(callee).with_sdp(OFFER_SDP).through(b2bua).send().await;
    let mut uas = callee.receive("INVITE").await;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    let answer = call.expect(200).await;
    let dialog = call.ack().await;
    callee.receive("ACK").await;
    (dialog, answer)
}
