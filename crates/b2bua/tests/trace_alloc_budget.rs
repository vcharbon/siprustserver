//! Allocation budget for the per-call trace machinery on an UNSAMPLED call
//! (ADR-0026).
//!
//! The explicit-guard discipline makes one promise that a code review cannot
//! check and a flamegraph cannot measure: a call that was not sampled pays
//! **nothing** for tracing existing. Every guarded emission site is exercised
//! here against an unsampled call and asserted at **zero** allocation events —
//! so a helper that ever formats its detail, serializes a body or clones a key
//! before consulting `call.sampled` fails this test rather than quietly costing
//! every call on the box.
//!
//! Run with output:
//! `cargo test -p b2bua --test trace_alloc_budget -- --nocapture`.

use std::net::SocketAddr;

use alloc_counter::{measure, CountingAlloc};
use b2bua::config::B2buaConfig;
use b2bua::initial_invite::build_initial_call;
use b2bua::trace;
use call::{Call, CallModelState};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

/// Counts every allocation the test binary performs. The single test below runs
/// on one thread and measures one region at a time.
#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Emissions per measured region: enough that the fixed cost of entering the
/// region is well under a per-operation rounding unit.
const ITERS: usize = 1000;

const WIRE: &[u8] = b"INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-alloc\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=alicetag\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: alloc-budget@10.0.0.9\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.9:5060>\r\n\
Content-Length: 0\r\n\r\n";

/// An ordinary call as the router holds it, with tracing NOT active — the
/// production shape of the 99.99% of calls the default 1e-4 rate does not draw.
fn unsampled_call() -> Call {
    let invite = match CustomParser::new().parse(WIRE).expect("fixture INVITE should parse") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("expected a request"),
    };
    let call = build_initial_call(
        &invite,
        SocketAddr::from(([10, 0, 0, 9], 5060)),
        &B2buaConfig::default(),
        0,
    );
    assert_eq!(call.sampled, None, "a fresh call is not sampled until the draw says so");
    call
}

/// One named emission site, as the router calls it.
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
fn an_unsampled_call_allocates_nothing_for_tracing() {
    let call = unsampled_call();
    let peer: SocketAddr = "10.0.0.9:5060".parse().expect("fixture address");

    // Every guarded site the b2bua drives per event, in the shape the router
    // calls it. `black_box` on the call keeps the optimizer from proving the
    // flag constant and deleting the whole site.
    let sites: [Site<'_>; 9] = [
        ("sampled", Box::new(|| assert!(!trace::sampled(std::hint::black_box(&call))))),
        ("sip.in", Box::new(|| trace::emit::sip_in(std::hint::black_box(&call), 0, peer, WIRE))),
        ("sip.out", Box::new(|| trace::emit::sip_out(std::hint::black_box(&call), 0, peer, WIRE))),
        (
            "rule.fired",
            Box::new(|| trace::emit::rule_fired(std::hint::black_box(&call), 0, "confirm-dialog")),
        ),
        (
            "rule.transition",
            Box::new(|| {
                trace::emit::rule_transition(
                    std::hint::black_box(&call),
                    0,
                    "confirm-dialog",
                    "transfer",
                    "idle",
                    "ringing",
                )
            }),
        ),
        (
            "call.transition",
            Box::new(|| {
                trace::emit::context_transition(
                    std::hint::black_box(&call),
                    0,
                    CallModelState::Active,
                    CallModelState::Terminating,
                )
            }),
        ),
        (
            "limiter",
            Box::new(|| trace::emit::limiter(std::hint::black_box(&call), 0, "admit", "trunk-1")),
        ),
        (
            "http round trip",
            Box::new(|| {
                trace::emit::round_trip(
                    std::hint::black_box(&call),
                    "/call/new",
                    0,
                    WIRE,
                    0,
                    "route",
                    WIRE,
                )
            }),
        ),
        (
            "detached handle",
            Box::new(|| {
                assert!(trace::emit::TraceHandle::of(std::hint::black_box(&call)).is_none())
            }),
        ),
    ];

    for (name, op) in &sites {
        let allocs = cost_of(op);
        println!("{name:<18} {allocs:>4} allocs / {ITERS} emissions");
        assert_eq!(allocs, 0, "unsampled `{name}` must not allocate; it allocated {allocs}");
    }

    // Hydration and release are per-call, not per-event, but they sit on the
    // same guard and must stay just as free.
    let mut call = unsampled_call();
    let call_ref = call.call_ref.clone();

    let allocs = cost_of(|| {
        trace::adopt_replicated(std::hint::black_box(&mut call), 0);
    });
    assert_eq!(allocs, 0, "hydrating an unsampled call must not allocate");

    let allocs = cost_of(|| {
        trace::traces().close(std::hint::black_box(&call_ref));
    });
    assert_eq!(allocs, 0, "releasing an untraced call must not allocate");
}
