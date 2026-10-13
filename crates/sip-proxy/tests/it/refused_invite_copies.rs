//! A new INVITE the proxy refuses itself draws one answer for every copy
//! (RFC 3261 §8.2.7): the copy of an INVITE refused by the self-gate or by a
//! worker's rate cap, re-sent because the 503 was lost, gets the same bytes and
//! is never forwarded, even when the refusing state has cleared by then. The
//! 503 carries a `Retry-After` and no `Reason`.

use crate::common;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::ProxySut;
use load_shed::simulated;
use scenario_harness::{Harness, WaiverScope};
use sip_clock::Clock;
use sip_message::parser::custom::CustomParser;
use sip_message::HeaderName;
use sip_message::{SipMessage, SipParser, SipResponse};
use sip_net::{PreIngressAction, PreIngressHook, UdpEndpoint};
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::WorkerRegistry;
use sip_proxy::self_gate::{EluCpsGate, ProxySelfGate, ProxySelfGateConfig};
use sip_proxy::strategy::{DecodeResult, RouteParams, SelectError, SelectOpts};
use sip_proxy::{ProxyAddr, ProxyCoreBuilder, ProxyMetrics, RoutingStrategy};
use sip_txn::IdGen;

const PROXY: &str = "127.0.0.1:5080";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const BRANCH: &str = "z9hG4bK-refused";

async fn spawn_proxy(
    h: &Harness,
    strategy: Arc<dyn RoutingStrategy>,
    gate: Option<Arc<dyn ProxySelfGate>>,
) -> ProxySut {
    let (ep, sock) = h.bind_sut("proxy", PROXY).await;
    let metrics = Arc::new(ProxyMetrics::new());
    let registry: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    let mut builder = ProxyCoreBuilder::new(ProxyAddr::from(sock), strategy, registry)
        .clock(Clock::test_at(0))
        .id_gen(Arc::new(IdGen::seeded(0xC0FFEE)))
        .metrics(metrics.clone());
    if let Some(gate) = gate {
        builder = builder.self_gate(gate);
    }
    ProxySut::from_task(sock, metrics, tokio::spawn(builder.build(ep).run()))
}

fn forward_to_bob() -> Arc<dyn RoutingStrategy> {
    let bob: SocketAddr = BOB.parse().unwrap();
    Arc::new(sip_proxy::ForwardAllStrategy::new(ProxyAddr::from(bob)))
}

fn invite() -> Vec<u8> {
    format!(
        "INVITE sip:bob@{BOB} SIP/2.0\r\n\
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

/// The §17.1.1.3 ACK for the proxy's own non-2xx final.
fn ack_for(resp: &SipResponse) -> Vec<u8> {
    format!(
        "ACK sip:bob@{BOB} SIP/2.0\r\n\
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

/// A caller whose first `lose` responses are lost on the way in. Each lost
/// datagram is kept, so the test can compare it with what arrives later.
async fn lossy_caller(h: &Harness, lose: u32) -> (Box<dyn UdpEndpoint>, Arc<Mutex<Vec<Vec<u8>>>>) {
    let lost = Arc::new(Mutex::new(Vec::new()));
    let kept = lost.clone();
    let hook: PreIngressHook = Arc::new(move |bytes: &[u8], _src, _depth| {
        let mut kept = kept.lock().unwrap();
        if (kept.len() as u32) < lose {
            kept.push(bytes.to_vec());
            PreIngressAction::Drop
        } else {
            PreIngressAction::Accept
        }
    });
    let (ep, _) = h.bind_sut_with_opts("alice", ALICE, sip_net::all_ua_roles(), Some(hook)).await;
    (ep, lost)
}

async fn recv_raw(client: &dyn UdpEndpoint) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("the caller gets a reply")
        .expect("queue open")
        .raw
        .to_vec()
}

fn parse(raw: &[u8]) -> SipResponse {
    match CustomParser::new().parse(raw).unwrap() {
        SipMessage::Response(resp) => resp,
        SipMessage::Request(_) => panic!("expected a response"),
    }
}

/// The 503 every copy drew, judged once: a `Retry-After`, no `Reason`.
fn assert_one_refusal(lost: &[Vec<u8>], last: &[u8]) -> SipResponse {
    for (i, earlier) in lost.iter().enumerate() {
        assert_eq!(
            String::from_utf8_lossy(earlier),
            String::from_utf8_lossy(last),
            "copy {i}'s 503 and the last one are the same bytes"
        );
    }
    let resp = parse(last);
    assert_eq!(resp.status(), 503);
    assert!(resp.raw(HeaderName::RetryAfter).next().is_some(), "the 503 carries Retry-After");
    assert!(resp.raw(HeaderName::Reason).next().is_none(), "the 503 carries no Reason");
    resp
}

/// A one-token bucket at 1 token/s, its token spent: the INVITE is refused,
/// its two 503s are lost, and the copy sent at 1.5 s (Timer A's second
/// retransmission) finds a token. It draws the same 503 and the token is not
/// spent on it.
#[tokio::test(start_paused = true)]
async fn a_copy_of_a_self_gate_refusal_draws_the_same_503_after_the_bucket_refills() {
    let h = Harness::with_transit_delay("refused-invite-copies-self-gate", 1);
    let (bob, _) = h.bind_sut("bob", BOB).await;
    let gate = Arc::new(EluCpsGate::new(
        Arc::new(simulated().0),
        ProxySelfGateConfig { cps_bucket_size: 1, cps_bucket_rate: 1, ..Default::default() },
    ));
    assert!(gate.try_admit_external().is_admit(), "another call spends the lone token");
    let proxy = spawn_proxy(&h, forward_to_bob(), Some(gate.clone())).await;
    let (alice, lost) = lossy_caller(&h, 2).await;

    alice.send_to(&invite(), proxy.addr()).await.unwrap();
    h.advance(Duration::from_millis(500)).await;
    alice.send_to(&invite(), proxy.addr()).await.unwrap();
    h.advance(Duration::from_millis(1000)).await;
    alice.send_to(&invite(), proxy.addr()).await.unwrap();
    let last = recv_raw(&*alice).await;

    let lost = lost.lock().unwrap().clone();
    assert_eq!(lost.len(), 2, "both earlier 503s were sent and lost");
    let resp = assert_one_refusal(&lost, &last);
    let m = gate.metrics();
    assert_eq!(m.rejected_cps_total, 1, "the refusal is counted once");
    assert_eq!(m.external_admitted_total, 1, "no copy is admitted");

    alice.send_to(&ack_for(&resp), proxy.addr()).await.unwrap();
    h.advance(Duration::from_millis(50)).await;
    assert!(bob.try_recv().is_none(), "no copy of the refused INVITE is forwarded");
    let _ = h.finish().await;
}

/// A strategy whose first selection fails with `refusal` and every later one
/// picks bob: the failure has cleared by the time a copy arrives.
struct RefuseThenForward {
    refusal: fn() -> SelectError,
    selections: AtomicU32,
}

impl RefuseThenForward {
    fn rate_capped() -> Arc<Self> {
        Arc::new(Self {
            refusal: || SelectError::RateCapExhausted {
                worker_id: "w1".into(),
                retry_after_sec: 2,
            },
            selections: AtomicU32::new(0),
        })
    }

    fn no_target() -> Arc<Self> {
        Arc::new(Self {
            refusal: || SelectError::NoTarget { reason: "no worker alive".into() },
            selections: AtomicU32::new(0),
        })
    }
}

#[async_trait]
impl RoutingStrategy for RefuseThenForward {
    fn name(&self) -> &str {
        "RefuseThenForward"
    }
    async fn select_for_new_dialog(
        &self,
        _msg: &SipMessage,
        _opts: SelectOpts,
    ) -> Result<ProxyAddr, SelectError> {
        if self.selections.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err((self.refusal)());
        }
        Ok(ProxyAddr::from(BOB.parse::<SocketAddr>().unwrap()))
    }
    async fn decode_stickiness(&self, _params: &RouteParams, _msg: &SipMessage) -> DecodeResult {
        DecodeResult::Unknown { is_emergency: false }
    }
    fn encode_stickiness(&self, _target: &ProxyAddr, _msg: &SipMessage) -> Option<RouteParams> {
        None
    }
    fn stickiness_primary<'p>(&self, _params: &'p RouteParams) -> Option<&'p str> {
        None
    }
}

/// The INVITE is refused at a worker's rate cap and its 503 is lost; the copy
/// at T1 draws the same 503 without a second selection.
#[tokio::test(start_paused = true)]
async fn a_copy_of_a_rate_cap_refusal_draws_the_same_503_without_a_new_selection() {
    let h = Harness::with_transit_delay("refused-invite-copies-rate-cap", 1);
    let (bob, _) = h.bind_sut("bob", BOB).await;
    let strategy = RefuseThenForward::rate_capped();
    let proxy = spawn_proxy(&h, strategy.clone(), None).await;
    let (alice, lost) = lossy_caller(&h, 1).await;

    alice.send_to(&invite(), proxy.addr()).await.unwrap();
    h.advance(Duration::from_millis(500)).await;
    alice.send_to(&invite(), proxy.addr()).await.unwrap();
    let last = recv_raw(&*alice).await;

    let lost = lost.lock().unwrap().clone();
    assert_eq!(lost.len(), 1, "the first 503 was sent and lost");
    let resp = assert_one_refusal(&lost, &last);
    assert_eq!(strategy.selections.load(Ordering::SeqCst), 1, "the copy is not selected afresh");
    assert_eq!(proxy.metrics.reject_count("worker_rate_capped"), 1, "counted once");

    alice.send_to(&ack_for(&resp), proxy.addr()).await.unwrap();
    h.advance(Duration::from_millis(50)).await;
    assert!(bob.try_recv().is_none(), "no copy of the refused INVITE is forwarded");
    let _ = h.finish().await;
}

/// No worker is selectable for the INVITE and its 503 is lost; the copy at T1
/// draws the same 503 though a worker is selectable by then.
#[tokio::test(start_paused = true)]
async fn a_copy_of_a_no_target_refusal_draws_the_same_503() {
    let h = Harness::with_transit_delay("refused-invite-copies-no-target", 1);
    let (bob, _) = h.bind_sut("bob", BOB).await;
    let strategy = RefuseThenForward::no_target();
    let proxy = spawn_proxy(&h, strategy.clone(), None).await;
    let (alice, lost) = lossy_caller(&h, 1).await;

    alice.send_to(&invite(), proxy.addr()).await.unwrap();
    h.advance(Duration::from_millis(500)).await;
    alice.send_to(&invite(), proxy.addr()).await.unwrap();
    let last = recv_raw(&*alice).await;

    let lost = lost.lock().unwrap().clone();
    assert_eq!(lost.len(), 1, "the first 503 was sent and lost");
    let resp = assert_one_refusal(&lost, &last);
    assert_eq!(strategy.selections.load(Ordering::SeqCst), 1, "the copy is not selected afresh");
    assert_eq!(proxy.metrics.reject_count("no_target_available"), 1, "counted once");

    alice.send_to(&ack_for(&resp), proxy.addr()).await.unwrap();
    h.advance(Duration::from_millis(50)).await;
    assert!(bob.try_recv().is_none(), "no copy of the refused INVITE is forwarded");
    let _ = h.finish().await;
}

/// The CANCEL of an INVITE this hop refused (RFC 3261 §9.1: the INVITE's
/// branch, sent-by, Call-ID, From-tag and CSeq number).
fn cancel() -> Vec<u8> {
    format!(
        "CANCEL sip:bob@{BOB} SIP/2.0\r\n\
Via: SIP/2.0/UDP {ALICE};branch={BRANCH}\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@127.0.0.1>;tag=t-{BRANCH}\r\n\
To: <sip:bob@127.0.0.1>\r\n\
Call-ID: {BRANCH}-call@127.0.0.1\r\n\
CSeq: 1 CANCEL\r\n\
Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// The INVITE is refused and its 503 lost; the caller gives up and CANCELs.
/// This hop answered the INVITE itself, so it is the UAS the CANCEL reaches
/// (§9.2): a 200 carrying the INVITE final's To-tag, nothing forwarded. The
/// INVITE's retransmission then draws its 503, which the caller ACKs.
#[tokio::test(start_paused = true)]
async fn a_cancel_of_a_refused_invite_draws_200_and_is_never_forwarded() {
    let h = Harness::with_transit_delay("refused-invite-cancel", 1);
    h.waive(
        WaiverScope::rule(
            "cancel-after-1xx",
            "alice CANCELs before any provisional (RFC 3261 §9.1): a stateless hop sends \
             none, so only such a caller ever CANCELs an INVITE it refused, the case under test",
        )
        .on_party("alice"),
    );
    let (bob, _) = h.bind_sut("bob", BOB).await;
    let strategy = RefuseThenForward::rate_capped();
    let proxy = spawn_proxy(&h, strategy.clone(), None).await;
    let (alice, lost) = lossy_caller(&h, 1).await;

    alice.send_to(&invite(), proxy.addr()).await.unwrap();
    h.advance(Duration::from_millis(100)).await;
    alice.send_to(&cancel(), proxy.addr()).await.unwrap();
    let cancel_ok = parse(&recv_raw(&*alice).await);
    assert_eq!(cancel_ok.status(), 200);
    assert_eq!(cancel_ok.cseq().method().as_str(), "CANCEL");

    h.advance(Duration::from_millis(400)).await;
    alice.send_to(&invite(), proxy.addr()).await.unwrap();
    let last = recv_raw(&*alice).await;
    let lost = lost.lock().unwrap().clone();
    let resp = assert_one_refusal(&lost, &last);
    assert_eq!(
        cancel_ok.raw(HeaderName::To).next(),
        resp.raw(HeaderName::To).next(),
        "the CANCEL's 200 carries the To-tag of the INVITE's final"
    );
    assert_eq!(strategy.selections.load(Ordering::SeqCst), 1, "the CANCEL selects nothing");

    alice.send_to(&ack_for(&resp), proxy.addr()).await.unwrap();
    h.advance(Duration::from_millis(50)).await;
    assert!(bob.try_recv().is_none(), "neither the CANCEL nor a copy is forwarded");
    let _ = h.finish().await;
}
