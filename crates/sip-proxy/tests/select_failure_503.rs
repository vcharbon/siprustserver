//! The 503 a failed worker selection answers a new INVITE with, on the wire:
//! the `Reason` text naming the failure and a `Retry-After` of at least 1 s.

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use common::spawn_proxy;
use scenario_harness::Harness;
use sip_message::header::{ParamValue, Reason, RetryAfter};
use sip_message::parser::custom::CustomParser;
use sip_message::HeaderName;
use sip_message::{SipMessage, SipParser};
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::WorkerRegistry;
use sip_proxy::strategy::{DecodeResult, RouteParams, SelectError, SelectOpts};
use sip_proxy::{ProxyAddr, RoutingStrategy};

const PROXY: &str = "127.0.0.1:5060";
const ALICE: &str = "127.0.0.1:5061";

/// Strategy double whose new-dialog selection always fails with the error it
/// was built with.
struct FailingSelect(fn() -> SelectError);

#[async_trait]
impl RoutingStrategy for FailingSelect {
    fn name(&self) -> &str {
        "FailingSelect"
    }
    async fn select_for_new_dialog(
        &self,
        _msg: &SipMessage,
        _opts: SelectOpts,
    ) -> Result<ProxyAddr, SelectError> {
        Err((self.0)())
    }
    async fn decode_stickiness(&self, _params: &RouteParams, _msg: &SipMessage) -> DecodeResult {
        DecodeResult::Unknown { is_emergency: false }
    }
    fn encode_stickiness(&self, _target: &ProxyAddr, _msg: &SipMessage) -> Option<RouteParams> {
        None
    }
}

const BRANCH: &str = "z9hG4bK-capped";

fn new_invite() -> Vec<u8> {
    format!(
        "INVITE sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP {ALICE};branch={BRANCH}\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@127.0.0.1>;tag=t-{BRANCH}\r\n\
To: <sip:bob@127.0.0.1>\r\n\
Call-ID: {BRANCH}-call@127.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@{ALICE}>\r\n\
Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// The §17.1.1.3 ACK for the proxy's own non-2xx final: the INVITE's branch
/// and CSeq number, To echoed from the final.
fn ack_for(resp: &sip_message::SipResponse) -> Vec<u8> {
    format!(
        "ACK sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP {ALICE};branch={BRANCH}\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@127.0.0.1>;tag=t-{BRANCH}\r\n\
To: {to}\r\n\
Call-ID: {BRANCH}-call@127.0.0.1\r\n\
CSeq: 1 ACK\r\n\
Content-Length: 0\r\n\r\n",
        to = resp.raw(HeaderName::To).next().expect("final carries To"),
    )
    .into_bytes()
}

/// A rate-capped worker whose hint is 0 still yields `Retry-After: 1`: a value
/// of 0 (RFC 3261 §20.33) asks for no wait, which a reject must never invite.
#[tokio::test]
async fn a_rate_cap_hint_of_zero_goes_on_the_wire_as_one_second() {
    let h = Harness::with_transit_delay("select-failure-503-zero", 0);
    let strategy: Arc<dyn RoutingStrategy> = Arc::new(FailingSelect(|| {
        SelectError::RateCapExhausted { worker_id: "w1".into(), retry_after_sec: 0 }
    }));
    let registry: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    let proxy = spawn_proxy(&h, PROXY, strategy, registry).await;
    let (client, _) = h.bind_sut("alice", ALICE).await;

    client.send_to(&new_invite(), proxy.addr()).await.unwrap();
    let reply = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
        .await
        .expect("client should get a reply")
        .expect("queue open");
    let SipMessage::Response(resp) = CustomParser::new().parse(&reply.raw).unwrap() else {
        panic!("expected a response");
    };

    assert_eq!(resp.status(), 503);
    let retry_after = resp.header::<RetryAfter>().expect("503 carries Retry-After").expect("reads");
    assert_eq!(retry_after.token(), "1", "a reject's Retry-After is floored at 1 s");
    let reason = resp.header::<Reason>().expect("503 carries Reason").expect("reads");
    assert_eq!(reason.param("text").and_then(ParamValue::as_str), Some("worker_rate_capped"));

    client.send_to(&ack_for(&resp), proxy.addr()).await.unwrap();
    let _ = h.finish().await;
}
