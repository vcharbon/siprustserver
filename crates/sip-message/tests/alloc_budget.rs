//! Allocation budget for the shared parse + proxy-forwarding paths.
//!
//! Every SIP-handling process in the workspace (b2bua worker, front proxy, load
//! generator) runs this same `sip-message` code, and CPU profiles put the
//! transient allocation it drives at the top of the on-CPU self-time ranking.
//! A flamegraph percentage is machine- and load-dependent; an **allocation
//! count** is not. This test measures, per operation, exactly how many
//! allocation events and requested bytes the parse and the proxy hop cost, and
//! asserts an upper budget — so a change that regresses allocation fails here,
//! and a change that removes it shows up as a number, not an impression.
//!
//! The measured cases are the ones `benches/sip_parser.rs` times, on the same
//! fixtures, so the allocation and the wall-clock story line up:
//!
//! - `decode/*` — parse only (raw bytes → `SipMessage`).
//! - `proxy_hop/*` — decode → clone → rewrite R-URI → insert Record-Route →
//!   serialize, the per-message cost of one forwarding hop.
//!
//! Run with output: `cargo test -p sip-message --test alloc_budget -- --nocapture`.

use alloc_counter::{measure, AllocCost, CountingAlloc};
use sip_message::{serialize, CustomParser, SipHeader, SipMessage, SipParser};

/// Counts every allocation the test binary performs. The single test below runs
/// on one thread and measures one region at a time, so the counters attribute
/// to the operation under measurement.
#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Operations per measured region: enough that the fixed cost of entering the
/// region is under a per-operation rounding unit.
const ITERS: usize = 1000;

const INVITE: &[u8] = b"INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP host.example.com;branch=z9hG4bK1\r\n\
Max-Forwards: 70\r\n\
From: Alice <sip:alice@example.com>;tag=1928\r\n\
To: Bob <sip:bob@example.com>\r\n\
Call-ID: a84b4c76e66710@pc33.example.com\r\n\
CSeq: 314159 INVITE\r\n\
Contact: <sip:alice@pc33.example.com>\r\n\
Content-Length: 0\r\n\r\n";

const OK_200: &[u8] = b"SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP host.example.com;branch=z9hG4bK1\r\n\
From: Alice <sip:alice@example.com>;tag=1928\r\n\
To: Bob <sip:bob@example.com>;tag=as83kf\r\n\
Call-ID: a84b4c76e66710@pc33.example.com\r\n\
CSeq: 314159 INVITE\r\n\
Contact: <sip:bob@pc33.example.com>\r\n\
Content-Length: 0\r\n\r\n";

/// An INVITE carrying an SDP offer — what a proxy actually forwards. Built at
/// runtime so Content-Length matches the body exactly.
fn invite_with_sdp() -> Vec<u8> {
    let sdp = "v=0\r\n\
o=alice 2890844526 2890844526 IN IP4 192.0.2.10\r\n\
s=-\r\n\
c=IN IP4 192.0.2.10\r\n\
t=0 0\r\n\
m=audio 49170 RTP/AVP 0 8 96\r\n\
a=rtpmap:0 PCMU/8000\r\n\
a=rtpmap:8 PCMA/8000\r\n\
a=rtpmap:96 opus/48000/2\r\n\
a=sendrecv\r\n";
    format!(
        "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-abc123\r\n\
Max-Forwards: 70\r\n\
From: Alice <sip:alice@example.com>;tag=1928\r\n\
To: Bob <sip:bob@example.com>\r\n\
Call-ID: a84b4c76e66710@pc33.example.com\r\n\
CSeq: 314159 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5060>\r\n\
Content-Type: application/sdp\r\n\
Content-Length: {}\r\n\r\n{}",
        sdp.len(),
        sdp
    )
    .into_bytes()
}

/// One proxy forwarding hop, identical to the bench's: decode → clone →
/// rewrite R-URI → add Record-Route → encode.
fn proxy_hop(parser: &CustomParser, raw: &[u8]) -> Vec<u8> {
    let msg = parser.parse(raw).expect("parse");
    let SipMessage::Request(req) = msg else { panic!("expected request") };
    let mut out = req.clone();
    out.uri = "sip:bob@192.0.2.99:5060".to_string().into();
    out.headers.insert(
        0,
        SipHeader { name: "Record-Route".to_string().into(), value: "<sip:proxy.example.com;lr>".to_string().into() },
    );
    serialize(&SipMessage::Request(out))
}

/// The per-operation allocation cost of running `op` [`ITERS`] times, after one
/// warm-up call so any one-time initialization is charged outside the region.
fn cost_per_op<T>(mut op: impl FnMut() -> T) -> AllocCost {
    drop(std::hint::black_box(op()));
    let (out, total) = measure(|| {
        let mut last = None;
        for _ in 0..ITERS {
            last = Some(std::hint::black_box(op()));
        }
        last
    });
    drop(out);
    total.per(ITERS)
}

/// Upper budget for one case: the measured pre-zero-copy cost rounded up ~20%.
struct Budget {
    case: &'static str,
    allocs: usize,
    bytes: usize,
}

/// The pre-zero-copy allocation budget. Each entry is the cost measured on the
/// owning (`String`-per-header-value) parser, rounded up by ~20% so ordinary
/// allocator/`std` variation does not fail the lane. The borrow-don't-own parse
/// path is expected to slash these — when it lands, re-measure and lower every
/// entry to the new cost, keeping the same headroom.
const BUDGETS: &[Budget] = &[
    Budget { case: "decode/invite", allocs: 87, bytes: 4300 },
    Budget { case: "decode/invite_sdp", allocs: 93, bytes: 5300 },
    Budget { case: "decode/200_ok", allocs: 82, bytes: 4800 },
    Budget { case: "proxy_hop/invite", allocs: 176, bytes: 9500 },
    Budget { case: "proxy_hop/invite_sdp", allocs: 189, bytes: 11400 },
];

fn budget(case: &str) -> &'static Budget {
    BUDGETS.iter().find(|b| b.case == case).expect("every measured case has a budget")
}

#[test]
fn parse_and_proxy_hop_stay_within_the_allocation_budget() {
    let parser = CustomParser::new();
    let invite_sdp = invite_with_sdp();

    let measured: Vec<(&str, AllocCost)> = vec![
        ("decode/invite", cost_per_op(|| parser.parse(INVITE).unwrap())),
        ("decode/invite_sdp", cost_per_op(|| parser.parse(&invite_sdp).unwrap())),
        ("decode/200_ok", cost_per_op(|| parser.parse(OK_200).unwrap())),
        ("proxy_hop/invite", cost_per_op(|| proxy_hop(&parser, INVITE))),
        ("proxy_hop/invite_sdp", cost_per_op(|| proxy_hop(&parser, &invite_sdp))),
    ];

    println!("\nsip-message allocation budget ({ITERS} ops per case)");
    println!("{:<22} {:>12} {:>12} {:>12} {:>12}", "case", "allocs/msg", "budget", "bytes/msg", "budget");
    for (case, cost) in &measured {
        let b = budget(case);
        println!("{case:<22} {:>12} {:>12} {:>12} {:>12}", cost.allocs, b.allocs, cost.bytes, b.bytes);
    }
    println!();

    for (case, cost) in &measured {
        let b = budget(case);
        assert!(
            cost.allocs <= b.allocs,
            "{case}: {} allocs/msg exceeds the {} budget",
            cost.allocs,
            b.allocs
        );
        assert!(
            cost.bytes <= b.bytes,
            "{case}: {} bytes/msg exceeds the {} budget",
            cost.bytes,
            b.bytes
        );
    }
}
