//! **A returning primary that takes its copy's write takes the consult the
//! copy awaits.** The primary dies undecided; its backup asks the decision
//! again and dials the callee. The primary restarts and reclaims the call
//! while the copy's INVITE rings: the copy plays on (ADR-0047 D7). The callee
//! refuses 486, and the copy sends the `/call/failure` consult and writes the
//! call. The primary takes that write, final and awaited consult with it, and
//! the copy stands down on the primary's write: one owner of the answer, the
//! primary, which sends the consult again and relays the 486 to the caller as
//! the answer says (ADR-0039 decision 5).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse,
};
use failover_harness::WorkerHealth;

use crate::departure_support::{bring_up_deciding, kill_announced, placed};
use crate::inflight_resend::{settle_to_one_cdr, split_mut};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";

/// How long the routing decision takes: past the kill.
const DECISION_DELAY: Duration = Duration::from_secs(1);
/// How long the failure consult takes: past the copy's stand-down.
const FAILURE_DELAY: Duration = Duration::from_millis(500);

/// Routes every call to bob with a callback context, so a refusal draws a
/// `/call/failure` consult, answered after [`FAILURE_DELAY`] with a relay.
struct Engine {
    failures: Arc<AtomicUsize>,
}

#[async_trait]
impl CallDecisionEngine for Engine {
    async fn new_call(&self, _req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        tokio::time::sleep(DECISION_DELAY).await;
        let mut route = route_to("127.0.0.1", 5070);
        route.callback_context = Some("ctx".into());
        Ok(NewCallResponse::Route(route))
    }

    async fn call_failure(
        &self,
        _req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        self.failures.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(FAILURE_DELAY).await;
        Ok(CallFailureResponse::Relay { label: None })
    }

    async fn call_refer(
        &self,
        _req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        Err(CallDecisionError::Unavailable("no transfer here".into()))
    }
}

#[tokio::test(start_paused = true)]
async fn the_primary_that_takes_its_copys_write_sends_the_consult_the_copy_awaited() {
    let name = "reclaim-takes-the-copys-consult";
    let mut fh = failover_harness::FailoverHarness::new(name, &["b1", "b2"]);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let failures = Arc::new(AtomicUsize::new(0));
    let engine = Arc::new(Engine { failures: failures.clone() });
    let (proxy, mut w_b1, mut w_b2) = bring_up_deciding(&mut fh, engine).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    call.expect(100).await;
    let (pri_ord, call_ref) = placed(&fh, &w_b1, &w_b2).await;
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    kill_announced(&mut fh, primary, survivor, &pri_ord, &proxy).await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // The primary restarts and reclaims the call while the copy's INVITE rings.
    let (primary, survivor) = split_mut(&pri_ord, &mut w_b1, &mut w_b2);
    fh.mark(&pri_ord, None, "reboot", "restart empty, higher gen, new pod IP");
    let new_addr = primary.reboot().await;
    proxy.set_address(&pri_ord, new_addr);
    fh.note_worker_rebound(&pri_ord, new_addr);
    survivor.simulate_peer_added(&pri_ord);
    for _ in 0..120 {
        fh.advance(Duration::from_millis(500)).await;
        if primary.is_ready() {
            break;
        }
    }
    assert!(primary.is_ready(), "the restarted primary became ready");
    proxy.set_health(&pri_ord, WorkerHealth::Alive);
    fh.advance(Duration::from_millis(500)).await;

    uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;
    let refused = call.expect(486).await;
    assert_eq!(refused.status(), 486, "the caller hears the callee's refusal, relayed");
    assert_eq!(failures.load(Ordering::SeqCst), 2, "the copy's consult, and the primary's again");
    settle_to_one_cdr(&fh, &[&alice, &bob], &[&w_b1, &w_b2], &call_ref).await;
    fh.assert_sip_rfc_clean(name);
    drop(proxy);
}
