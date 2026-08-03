//! Allocation budget for the proxy's per-call trace tier when NOTHING is
//! sampled (ADR-0026).
//!
//! The proxy touches every datagram of every call on the box, so the tier makes
//! one promise a code review cannot check and a flamegraph cannot measure: with
//! no call sampled, the per-packet path is a single predicted branch — no
//! lookup, no formatting, no allocation. Every emission site is exercised here
//! against an untraced registry and asserted at **zero** allocation events, so a
//! helper that ever formats its detail or copies a Call-ID before consulting the
//! flag fails here instead of quietly costing every packet in production.
//!
//! Run with output:
//! `cargo test -p sip-proxy --test trace_alloc_budget -- --nocapture`.

use std::sync::Arc;

use alloc_counter::{measure, CountingAlloc};
use observe::{CallIdentity, RateDraw, SampleAdmission, TokenBucket};
use sip_message::sniff;
use sip_proxy::observability::metrics::{Face, RoutingDecisionKind};
use sip_proxy::trace::emit::{self, RouteFacts};
use sip_proxy::{ProxyAddr, ProxyTraces};

/// Counts every allocation the test binary performs. The single test below runs
/// on one thread and measures one region at a time.
#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Emissions per measured region: enough that the fixed cost of entering the
/// region is well under a per-operation rounding unit.
const ITERS: usize = 1000;

const INVITE: &[u8] = b"INVITE sip:bob@10.0.0.2:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-alloc\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=alicetag\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: alloc-budget@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5060>\r\n\
Content-Length: 0\r\n\r\n";

const CALL_ID: &str = "alloc-budget@10.0.0.1";

/// The production shape of a proxy that draws at 1e-4 and has drawn nothing:
/// an exporter is configured (so the machinery is live), yet no call is traced.
fn untraced() -> Arc<ProxyTraces> {
    Arc::new(ProxyTraces::new(
        SampleAdmission::new(true, 0.0, 200, RateDraw::seeded(1), TokenBucket::default_at(0)),
        true,
    ))
}

/// One named emission site, as the data path calls it.
type Site<'a> = (&'a str, Box<dyn Fn() + 'a>);

/// The allocation cost of running `op` [`ITERS`] times, after one warm-up call
/// so any one-time initialization is charged outside the region.
fn cost_of(mut op: impl FnMut()) -> usize {
    op();
    let ((), total) = measure(|| {
        for _ in 0..ITERS {
            op();
            std::hint::black_box(());
        }
    });
    total.allocs
}

#[test]
fn an_untraced_proxy_allocates_nothing_per_packet() {
    let traces = untraced();
    assert!(
        !traces.activate(CALL_ID, CallIdentity { call_id: CALL_ID, from_tag: "a", to_tag: "" }, None, 0),
        "the 0.0 draw refuses every call — nothing is sampled",
    );
    let src = "10.0.0.1:5060".parse().expect("fixture address");
    let target = ProxyAddr::new("10.0.0.2", 5070);
    let facts = || RouteFacts {
        decision: RoutingDecisionKind::SelectNew,
        target: &target,
        face: Some(Face::Internal),
        stickiness: None,
    };
    let t = || std::hint::black_box(traces.as_ref());

    let sites: [Site<'_>; 6] = [
        ("any_sampled", Box::new(|| assert!(!t().any_sampled()))),
        ("sip.in", Box::new(|| emit::sip_in(t(), CALL_ID, 0, src, INVITE))),
        ("response sip.in", Box::new(|| emit::response_in(t(), CALL_ID, 0, 200, "INVITE", INVITE))),
        ("sip.out + route", Box::new(|| emit::forwarded(t(), CALL_ID, 0, facts(), INVITE))),
        ("route.shed", Box::new(|| emit::shed(t(), CALL_ID, 0, "proxy_overload_cps"))),
        ("close", Box::new(|| t().close(CALL_ID))),
    ];

    for (name, op) in &sites {
        let allocs = cost_of(op);
        println!("{name:<18} {allocs:>4} allocs / {ITERS} emissions");
        assert_eq!(allocs, 0, "untraced `{name}` must not allocate; it allocated {allocs}");
    }

    // The raw INVITE scan the sampling decision runs on a lab-gated process is
    // on the same path, and reads the datagram in place.
    let allocs = cost_of(|| {
        std::hint::black_box(sniff::trace_sample_rate(std::hint::black_box(INVITE)));
    });
    assert_eq!(allocs, 0, "the raw X-Trace-Sample scan must not allocate; it allocated {allocs}");
}
