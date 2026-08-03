//! End-to-end: a SAMPLED call records its datagrams and routing facts on ONE
//! proxy-side root span, and that span closes with the call (ADR-0026).
//!
//! The scenario is the canonical complete callflow — alice → ProxyCore → bob,
//! INVITE/180/200/ACK/BYE — so the wire stays RFC-compliant and the call reaches
//! a terminal state; the trace assertions ride on top. This is a dedicated test
//! OF the trace machinery, so it may assert on the captured subscriber buffer;
//! ordinary scenario tests never do (the `Recorder` is their oracle).
//!
//! The proxy adds NOTHING to the wire for tracing: sampling is independent per
//! process and correlation is by `sip.call_id` alone, which the RFC audit at
//! `finish()` and the header assertions below both hold it to.

mod common;

use std::sync::Arc;

use common::{forward_all, spawn_proxy_with_traces};
use observe::{RateDraw, SampleAdmission, TokenBucket};
use scenario_harness::Harness;
use sip_message::HeaderName;
use sip_proxy::ProxyTraces;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49180 RTP/AVP 0\r\n";

/// A gate that samples every call: an exporter is "configured", the draw always
/// wins, and the header is not honored (this process did not opt in).
fn sample_everything() -> Arc<ProxyTraces> {
    Arc::new(ProxyTraces::new(
        SampleAdmission::new(true, 1.0, 200, RateDraw::seeded(1), TokenBucket::default_at(0)),
        false,
    ))
}

#[tokio::test]
async fn a_sampled_call_records_its_datagrams_and_routing_on_one_span() {
    let (_log_guard, log) = observe::test_buffer();

    let h = Harness::new("proxy-per-call-trace")
        .describe("alice → ProxyCore(ForwardAll) → bob, with the proxy tracing the call.");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let (strategy, registry) = forward_all(bob.addr());
    let traces = sample_everything();
    let proxy =
        spawn_proxy_with_traces(&h, "127.0.0.1:5080", strategy, registry, traces.clone()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    let mut uas = bob.receive("INVITE").await;
    let forwarded = uas.request();
    assert!(
        forwarded.raw(HeaderName::from("X-Trace-Sample")).next().is_none(),
        "the proxy adds no sampling header of its own",
    );
    assert!(
        !forwarded.has(&HeaderName::from("traceparent")),
        "no span context ever rides the SIP wire",
    );
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    assert_eq!(traces.active(), 1, "the call holds exactly one root span at the proxy");

    // ── The call's story, as this hop saw it ────────────────────────────────
    let span = log.spans_matching("sip.call");
    assert_eq!(span.len(), 1, "one root span per call per process");
    assert!(span[0].contains("sip.call_id="), "every span carries the correlation key");

    let sip_in = log.matching("kind=sip.in");
    assert!(sip_in.iter().any(|e| e.contains("INVITE sip:")), "the INVITE as it arrived, raw");
    assert!(sip_in.iter().any(|e| e.contains("200 for INVITE")), "and the answer it drew");
    assert!(sip_in.iter().all(|e| e.contains("at_ms=")), "every fact is timestamped");
    assert!(
        log.matching("kind=sip.out").iter().any(|e| e.contains("INVITE sip:")),
        "the datagram the proxy put on the wire",
    );
    let decision = log.matching("kind=route.decision");
    assert!(
        decision.iter().any(|e| e.contains("select_new") && e.contains("127.0.0.1:5070")),
        "the routing facts name the hop that was chosen: {:?}",
        decision.iter().map(|e| e.line()).collect::<Vec<_>>(),
    );

    // ── Teardown: the BYE final closes the span ─────────────────────────────
    let mut bye = dialog.bye().await;
    let mut bob_bye = bob.receive("BYE").await;
    bob_bye.respond(200, "OK").await;
    bye.expect(200).await;

    assert_eq!(traces.active(), 0, "the observed BYE final closed the span, freeing its slot");

    let report = h.finish().await;
    assert!(report.entries().iter().all(|e| e.delivered), "all hops delivered");
}
