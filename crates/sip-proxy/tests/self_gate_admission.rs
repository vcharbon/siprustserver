//! Proxy-self-gate admission suite — the on-wire half of the migration/14 gate.
//!
//! The 13 `self_gate::tests` unit tests prove [`EluCpsGate`] returns the right
//! [`AdmitDecision`]; this suite pins the **glue** in `core/request.rs` that turns
//! a gate decision into the user-visible contract: a rejected NEW external
//! new-dialog INVITE gets a `503 Service Unavailable` with `Retry-After: <n>` and
//! `Reason: SIP;cause=503;text="<phrase>"`, while emergency / worker-outbound
//! new-dialog INVITEs BYPASS the gate (no 503) and increment the right bypass
//! counter. The reply is observed exactly the way `transit_only.rs`'s
//! `max_forwards_zero_returns_483` does — a bound client endpoint `recv()`ing the
//! proxy's response off the harness fabric.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use common::ProxySut;
use scenario_harness::Harness;
use sip_clock::Clock;
use sip_message::message_helpers::get_header;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerRegistry};
use sip_proxy::self_gate::{
    simulated, AdmitDecision, BypassKind, EluCpsGate, ProxySelfGate, ProxySelfGateConfig,
};
use sip_proxy::{ProxyAddr, ProxyCoreBuilder, ProxyMetrics, RoutingStrategy};
use sip_txn::IdGen;

const PROXY: &str = "127.0.0.1:5080";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
// A registered worker — its identity is what makes a new-dialog INVITE whose top
// Via sent-by matches it classify as worker-outbound (the internal-bypass arm).
const WORKER: &str = "127.0.0.1:5090";

/// Bind + spawn a real `ProxyCore` on the harness fabric with a caller-supplied
/// gate (the shared `common::spawn_proxy` always-admits). Seeded `IdGen` + test
/// clock, exactly like `spawn_proxy`, so the only variable is the gate.
async fn spawn_proxy_with_gate(
    h: &Harness,
    strategy: Arc<dyn RoutingStrategy>,
    registry: Arc<dyn WorkerRegistry>,
    gate: Arc<dyn ProxySelfGate>,
) -> ProxySut {
    let (ep, sock) = h.bind_sut("proxy", PROXY).await;
    let metrics = Arc::new(ProxyMetrics::new());
    let core = ProxyCoreBuilder::new(ProxyAddr::from(sock), strategy, registry)
        .clock(Clock::test_at(0))
        .id_gen(Arc::new(IdGen::seeded(0xC0FFEE)))
        .metrics(metrics.clone())
        .self_gate(gate)
        .build(ep);
    ProxySut::from_task(sock, metrics, tokio::spawn(core.run()))
}

/// ForwardAll → bob, with a one-entry registry so a worker-Via INVITE can be
/// classified worker-outbound.
fn forward_all_with_worker() -> (Arc<dyn RoutingStrategy>, Arc<dyn WorkerRegistry>) {
    let bob: SocketAddr = BOB.parse().unwrap();
    let strategy: Arc<dyn RoutingStrategy> = Arc::new(sip_proxy::ForwardAllStrategy::new(ProxyAddr::from(bob)));
    let registry: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
        "w1",
        ProxyAddr::from(WORKER.parse::<SocketAddr>().unwrap()),
    )]));
    (strategy, registry)
}

/// A new-dialog INVITE (no To-tag) from `from_via` toward bob through the proxy.
/// `rph` injects a `Resource-Priority` header (`Some("esnet.0")` → emergency).
/// The From-tag is derived from `branch` — each call is its own dialog, the way
/// a real UAC mints a fresh tag per call (two calls sharing a From-tag confuse
/// the per-dialog RFC audit projection at the proxy bind).
fn new_invite(from_via: &str, branch: &str, rph: Option<&str>) -> Vec<u8> {
    let rph_line = rph.map(|v| format!("Resource-Priority: {v}\r\n")).unwrap_or_default();
    format!(
        "INVITE sip:bob@{BOB} SIP/2.0\r\n\
Via: SIP/2.0/UDP {from_via};branch={branch}\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@127.0.0.1>;tag=t-{branch}\r\n\
To: <sip:bob@127.0.0.1>\r\n\
Call-ID: {branch}-call@127.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@{from_via}>\r\n\
{rph_line}\
Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// `recv()` the proxy's reply off a bound client endpoint and parse it (the
/// `max_forwards_zero_returns_483` shape).
async fn recv_response(client: &dyn sip_net::UdpEndpoint) -> sip_message::SipResponse {
    let reply = tokio::time::timeout(std::time::Duration::from_secs(2), client.recv())
        .await
        .expect("client should get a reply")
        .expect("queue open");
    match CustomParser::new().parse(&reply.raw).unwrap() {
        SipMessage::Response(resp) => resp,
        _ => panic!("expected a response"),
    }
}

// ── The 503 wire contract — the slice's user-visible output ──────────────────
//
// A force-reject gate makes EVERY external new-dialog INVITE shed. This pins the
// reject branch of `core/request.rs` (L262-272): RouteOutcome.decision==Reject,
// the source gets a 503 with Retry-After=<gate value> and the exact Reason
// header, AND the `reason.unwrap_or_else("proxy_overload_cps")` fallback fires
// when the gate hands back a `None` reason.

/// Gate that always rejects, with a configurable Reason/Retry-After (or `None`
/// reason to exercise the request-path fallback).
struct ForceRejectGate {
    reason: Option<&'static str>,
    retry_after_sec: u32,
    tries: AtomicU32,
}

impl ForceRejectGate {
    fn new(reason: Option<&'static str>, retry_after_sec: u32) -> Arc<Self> {
        Arc::new(Self { reason, retry_after_sec, tries: AtomicU32::new(0) })
    }
}

impl ProxySelfGate for ForceRejectGate {
    fn try_admit_external(&self) -> AdmitDecision {
        self.tries.fetch_add(1, Ordering::SeqCst);
        AdmitDecision { admit: false, reason: self.reason.map(String::from), retry_after_sec: self.retry_after_sec }
    }
}

/// Drive a single new-dialog external INVITE through a gate that rejects with the
/// given (reason, retry) and assert the on-wire 503 the source receives.
async fn assert_wire_503(name: &str, gate_reason: Option<&'static str>, retry: u32, expect_reason_text: &str) {
    let h = Harness::with_transit_delay(name, 0);
    let (bob_ep, _bob_addr) = h.bind_sut("bob", BOB).await;
    let (strategy, registry) = forward_all_with_worker();
    let gate = ForceRejectGate::new(gate_reason, retry);
    let proxy = spawn_proxy_with_gate(&h, strategy, registry, gate.clone()).await;
    let (client, _client_addr) = h.bind_sut("alice", ALICE).await;

    client.send_to(&new_invite(ALICE, "z9hG4bK-rej", None), proxy.addr()).await.unwrap();
    let resp = recv_response(&*client).await;

    assert_eq!(resp.status, 503, "a rejected new external INVITE must get a 503");
    assert_eq!(resp.reason, "Service Unavailable");
    assert_eq!(
        get_header(&resp.headers, "retry-after"),
        Some(retry.to_string().as_str()),
        "503 must carry the gate's Retry-After"
    );
    assert_eq!(
        get_header(&resp.headers, "reason"),
        Some(format!("SIP;cause=503;text=\"{expect_reason_text}\"").as_str()),
        "503 must carry the exact Reason phrase"
    );
    assert_eq!(gate.tries.load(Ordering::SeqCst), 1, "the gate is consulted exactly once");

    // §17.1.1.3: ACK the shed 503 (same branch/CSeq as the INVITE, To echoed
    // from the response). The proxy generated that final itself, so it absorbs
    // the ACK at this hop — bob still sees nothing (asserted below).
    client.send_to(&ack_for_reject(ALICE, "z9hG4bK-rej", &resp), proxy.addr()).await.unwrap();

    // The over-the-cap INVITE was NEVER forwarded (and neither was its ACK).
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(bob_ep.try_recv().is_none(), "a shed INVITE must not reach the backend");
    let _ = h.finish().await;
}

/// The §17.1.1.3 ACK for a proxy-self-generated non-2xx INVITE final: reuses
/// the INVITE's top-Via branch + CSeq number (method ACK), From/Call-ID
/// unchanged, To echoed from the final (it carries the proxy's minted tag).
fn ack_for_reject(from_via: &str, branch: &str, resp: &sip_message::SipResponse) -> Vec<u8> {
    format!(
        "ACK sip:bob@{BOB} SIP/2.0\r\n\
Via: SIP/2.0/UDP {from_via};branch={branch}\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@127.0.0.1>;tag=t-{branch}\r\n\
To: {to}\r\n\
Call-ID: {branch}-call@127.0.0.1\r\n\
CSeq: 1 ACK\r\n\
Content-Length: 0\r\n\r\n",
        to = get_header(&resp.headers, "to").expect("final carries To"),
    )
    .into_bytes()
}

#[tokio::test]
async fn rejected_invite_returns_503_with_cps_reason_and_retry_after() {
    assert_wire_503("self-gate-503-cps", Some("proxy_overload_cps"), 3, "proxy_overload_cps").await;
}

#[tokio::test]
async fn rejected_invite_returns_503_with_elu_reason_and_retry_after() {
    assert_wire_503("self-gate-503-elu", Some("proxy_overload_elu"), 1, "proxy_overload_elu").await;
}

#[tokio::test]
async fn rejected_invite_without_a_reason_falls_back_to_proxy_overload_cps() {
    // The gate handed back `reason: None`; the request path's
    // `unwrap_or_else(|| "proxy_overload_cps")` fallback must fill it.
    assert_wire_503("self-gate-503-fallback", None, 7, "proxy_overload_cps").await;
}

// ── End-to-end through the REAL EluCpsGate (not a double) ────────────────────
//
// Pins that the real gate, wired into the proxy, produces the documented wire
// 503 for BOTH rejection reasons — the CPS bucket draining and the ELU EWMA
// crossing critical — so a Reason/Retry-After drift in either the gate or the
// glue is caught together.

#[tokio::test(start_paused = true)]
async fn real_gate_cps_drain_yields_a_wire_503_cps() {
    let h = Harness::with_transit_delay("self-gate-real-cps", 1);
    let (_bob_ep, _bob_addr) = h.bind_sut("bob", BOB).await;
    let (strategy, registry) = forward_all_with_worker();
    // Bucket size 1, rate 0 → the second external INVITE sheds with the TS 60 s
    // Retry-After fallback (rate 0 + empty bucket).
    let gate = Arc::new(EluCpsGate::new(
        Arc::new(simulated().0),
        ProxySelfGateConfig { cps_bucket_size: 1, cps_bucket_rate: 0, elu_critical: 0.8, ..Default::default() },
    ));
    let proxy = spawn_proxy_with_gate(&h, strategy, registry, gate.clone()).await;
    let (client, _client_addr) = h.bind_sut("alice", ALICE).await;

    // First INVITE drains the lone token (admitted → forwarded to bob).
    client.send_to(&new_invite(ALICE, "z9hG4bK-ok", None), proxy.addr()).await.unwrap();
    let bob_uas = _bob_ep;
    // Pump the paused clock until the admitted INVITE lands at bob (the
    // simulated fabric delivers on a spawned sleep(transit_delay); a fixed
    // single advance would race it). Then it's drained, so it can't be confused
    // with the shed second one. Bounded so a routing regression fails fast.
    let mut delivered = false;
    for _ in 0..20 {
        if bob_uas.try_recv().is_some() {
            delivered = true;
            break;
        }
        tokio::time::advance(std::time::Duration::from_millis(5)).await;
        tokio::task::yield_now().await;
    }
    assert!(delivered, "the first (admitted) INVITE reaches bob");

    // Second INVITE: bucket empty → 503 proxy_overload_cps, Retry-After 60.
    client.send_to(&new_invite(ALICE, "z9hG4bK-shed", None), proxy.addr()).await.unwrap();
    let resp = recv_response(&*client).await;
    assert_eq!(resp.status, 503);
    assert_eq!(get_header(&resp.headers, "reason"), Some("SIP;cause=503;text=\"proxy_overload_cps\""));
    assert_eq!(get_header(&resp.headers, "retry-after"), Some("60"));
    assert_eq!(gate.metrics().rejected_cps_total, 1);

    // §17.1.1.3: ACK the shed 503 — absorbed at the proxy (self-generated).
    client.send_to(&ack_for_reject(ALICE, "z9hG4bK-shed", &resp), proxy.addr()).await.unwrap();

    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    assert!(bob_uas.try_recv().is_none(), "the shed INVITE must not reach bob");
    let _ = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn real_gate_elu_over_critical_yields_a_wire_503_elu() {
    let h = Harness::with_transit_delay("self-gate-real-elu", 1);
    let (bob_ep, _bob_addr) = h.bind_sut("bob", BOB).await;
    let (strategy, registry) = forward_all_with_worker();
    let (sampler, ctl) = simulated();
    let gate = Arc::new(EluCpsGate::new(
        Arc::new(sampler),
        ProxySelfGateConfig { cps_bucket_size: 50, cps_bucket_rate: 100, elu_critical: 0.8, ..Default::default() },
    ));
    // Peg the ELU above critical and seat the EWMA there (two samples from 0:
    // 0.2*0.9 path needs a few; set 1.0 then sample twice to clear 0.8).
    ctl.set_elu(1.0);
    gate.sample();
    gate.sample();
    assert!(gate.elu_ewma() > 0.8, "EWMA must be seated above critical, got {}", gate.elu_ewma());

    let proxy = spawn_proxy_with_gate(&h, strategy, registry, gate.clone()).await;
    let (client, _client_addr) = h.bind_sut("alice", ALICE).await;

    client.send_to(&new_invite(ALICE, "z9hG4bK-elu", None), proxy.addr()).await.unwrap();
    let resp = recv_response(&*client).await;
    assert_eq!(resp.status, 503);
    assert_eq!(get_header(&resp.headers, "reason"), Some("SIP;cause=503;text=\"proxy_overload_elu\""));
    // ELU rejection carries Retry-After: 1.
    assert_eq!(get_header(&resp.headers, "retry-after"), Some("1"));
    assert_eq!(gate.metrics().rejected_elu_total, 1);

    // §17.1.1.3: ACK the shed 503 — absorbed at the proxy (self-generated).
    client.send_to(&ack_for_reject(ALICE, "z9hG4bK-elu", &resp), proxy.addr()).await.unwrap();

    tokio::time::advance(std::time::Duration::from_millis(50)).await;
    assert!(bob_ep.try_recv().is_none(), "an ELU-shed INVITE must not reach bob");
    let _ = h.finish().await;
}

// ── Bypass arms: the three-way classification end-to-end (TS ProxyCore L852-855)
//
// A force-reject gate would 503 anything the request path lets through. So if an
// emergency / worker-outbound new-dialog INVITE reaches bob (no 503) AND the
// gate's bypass counter ticks, the bypass branch was taken — pinning that
// neither path even calls `try_admit_external`.

/// Gate that rejects every admission attempt but tallies bypasses — so a 503
/// proves the gate was consulted, and a forward proves it was bypassed.
struct BypassProbeGate {
    admit_tries: AtomicU32,
    emergency: AtomicU32,
    internal: AtomicU32,
}

impl BypassProbeGate {
    fn new() -> Arc<Self> {
        Arc::new(Self { admit_tries: AtomicU32::new(0), emergency: AtomicU32::new(0), internal: AtomicU32::new(0) })
    }
}

impl ProxySelfGate for BypassProbeGate {
    fn try_admit_external(&self) -> AdmitDecision {
        self.admit_tries.fetch_add(1, Ordering::SeqCst);
        AdmitDecision { admit: false, reason: Some("proxy_overload_cps".into()), retry_after_sec: 5 }
    }
    fn note_bypass(&self, kind: BypassKind) {
        match kind {
            BypassKind::Emergency => self.emergency.fetch_add(1, Ordering::SeqCst),
            BypassKind::Internal => self.internal.fetch_add(1, Ordering::SeqCst),
        };
    }
}

#[tokio::test]
async fn emergency_new_dialog_invite_bypasses_the_gate() {
    let h = Harness::with_transit_delay("self-gate-bypass-emergency", 0);
    let (bob_ep, _bob_addr) = h.bind_sut("bob", BOB).await;
    let (strategy, registry) = forward_all_with_worker();
    let gate = BypassProbeGate::new();
    let proxy = spawn_proxy_with_gate(&h, strategy, registry, gate.clone()).await;
    let (client, _client_addr) = h.bind_sut("alice", ALICE).await;

    // Emergency INVITE (Resource-Priority: esnet.0) — must bypass, never 503.
    client.send_to(&new_invite(ALICE, "z9hG4bK-emg", Some("esnet.0")), proxy.addr()).await.unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(bob_ep.try_recv().is_some(), "an emergency INVITE must bypass the gate and reach bob");
    assert!(client.try_recv().is_none(), "an emergency INVITE must NOT be 503'd");
    assert_eq!(gate.admit_tries.load(Ordering::SeqCst), 0, "the gate must not even be consulted");
    assert_eq!(gate.emergency.load(Ordering::SeqCst), 1, "the emergency bypass must be counted");
    assert_eq!(gate.internal.load(Ordering::SeqCst), 0);
    let _ = h.finish().await;
}

#[tokio::test]
async fn worker_outbound_new_dialog_invite_bypasses_the_gate() {
    let h = Harness::with_transit_delay("self-gate-bypass-internal", 0);
    let (bob_ep, _bob_addr) = h.bind_sut("bob", BOB).await;
    let (strategy, registry) = forward_all_with_worker();
    let gate = BypassProbeGate::new();
    let proxy = spawn_proxy_with_gate(&h, strategy, registry, gate.clone()).await;
    // The "alice" endpoint here stands in for the worker: its top Via sent-by is
    // the registered worker address, which is the SNAT-immune worker-outbound
    // discriminator (a worker b-leg INVITE).
    let (worker_client, _wc_addr) = h.bind_sut("worker", WORKER).await;

    worker_client.send_to(&new_invite(WORKER, "z9hG4bK-int", None), proxy.addr()).await.unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(bob_ep.try_recv().is_some(), "a worker-outbound INVITE must bypass the gate and reach bob");
    assert!(worker_client.try_recv().is_none(), "a worker-outbound INVITE must NOT be 503'd");
    assert_eq!(gate.admit_tries.load(Ordering::SeqCst), 0, "the gate must not even be consulted");
    assert_eq!(gate.internal.load(Ordering::SeqCst), 1, "the internal (worker) bypass must be counted");
    assert_eq!(gate.emergency.load(Ordering::SeqCst), 0);
    let _ = h.finish().await;
}
