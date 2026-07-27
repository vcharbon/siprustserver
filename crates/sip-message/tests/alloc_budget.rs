//! Allocation budget for the shared parse, forwarding and origination paths.
//!
//! Every SIP-handling process in the workspace (b2bua worker, front proxy, load
//! generator) runs this same `sip-message` code, and CPU profiles put the
//! transient allocation it drives at the top of the on-CPU self-time ranking.
//! A flamegraph percentage is machine- and load-dependent; an **allocation
//! count** is not. This test measures, per operation, exactly how many
//! allocation events and requested bytes each path costs, and asserts an upper
//! budget — so a change that regresses allocation fails here, and a change that
//! removes it shows up as a number, not an impression.
//!
//! The cases and their fixtures live in [`perf`], shared with
//! `benches/sip_parser.rs`, so the allocation and the wall-clock story line up.
//!
//! Run with output: `cargo test -p sip-message --test alloc_budget -- --nocapture`.

mod perf;

use alloc_counter::{measure, AllocCost, CountingAlloc};
use sip_message::generators::{
    generate_in_dialog_request, generate_out_of_dialog_request, generate_response, InDialogMethod,
    OutOfDialogMethod,
};
use sip_message::{CustomParser, SipParser};

use perf::BlankInvite;

/// Counts every allocation the test binary performs. The single test below runs
/// on one thread and measures one region at a time, so the counters attribute
/// to the operation under measurement.
#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Operations per measured region: enough that the fixed cost of entering the
/// region is under a per-operation rounding unit.
const ITERS: usize = 1000;

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

/// Upper budget for one case: the measured cost rounded up ~20%, so ordinary
/// allocator and `std` variation does not fail the lane while a real regression
/// does.
struct Budget {
    case: &'static str,
    allocs: usize,
    bytes: usize,
}

/// The allocation budget, re-baselined on the ADR-0025 header model.
///
/// ADR-0025's guardrails target ≤ ~10 allocs per parse and ≤ ~12 per
/// thawed-draft hop. `hop/stamp_rport` — the smallest edit a hop can make —
/// meets the hop target; `decode/*` sits two above its own and
/// `hop/rewrite_set` (six edits, two Route pops) well above. A budget states
/// what the code costs today, never what would let it pass, so those two carry
/// their measured cost and the gap is tracked in the migration log.
const BUDGETS: &[Budget] = &[
    Budget { case: "decode/invite", allocs: 14, bytes: 4090 },
    Budget { case: "decode/invite_sdp", allocs: 15, bytes: 4490 },
    Budget { case: "decode/200_ok", allocs: 14, bytes: 3890 },
    Budget { case: "hop/rewrite_set", allocs: 34, bytes: 10070 },
    Budget { case: "hop/stamp_rport", allocs: 9, bytes: 2810 },
    Budget { case: "proxy_hop/invite", allocs: 25, bytes: 6870 },
    Budget { case: "proxy_hop/invite_sdp", allocs: 26, bytes: 7750 },
    Budget { case: "build/blank_draft", allocs: 32, bytes: 8460 },
    Budget { case: "build/invite_sdp", allocs: 54, bytes: 8210 },
    Budget { case: "build/bye", allocs: 60, bytes: 5020 },
    Budget { case: "build/response_200", allocs: 38, bytes: 7520 },
];

fn budget(case: &str) -> &'static Budget {
    BUDGETS.iter().find(|b| b.case == case).expect("every measured case has a budget")
}

#[test]
fn parse_build_and_hop_stay_within_the_allocation_budget() {
    let parser = CustomParser::new();
    let invite_sdp = perf::invite_with_sdp();
    let in_dialog = perf::in_dialog_invite_with_sdp();
    let sdp = perf::body_of(&invite_sdp);

    // Parsed outside the measured region: the `hop/*` cases charge the thawed
    // draft alone, which is the shape ADR-0025 guardrail 2 budgets.
    let inbound = perf::request(&parser, &in_dialog);
    let parsed_invite = perf::request(&parser, perf::INVITE);

    let blank = BlankInvite::new(&sdp);
    let dialog = perf::build_dialog();
    let invite_opts = perf::invite_opts(&sdp);
    let bye_opts = perf::bye_opts();
    let response_opts = perf::response_opts();

    let measured: Vec<(&str, AllocCost)> = vec![
        ("decode/invite", cost_per_op(|| parser.parse(perf::INVITE).unwrap())),
        ("decode/invite_sdp", cost_per_op(|| parser.parse(&invite_sdp).unwrap())),
        ("decode/200_ok", cost_per_op(|| parser.parse(perf::OK_200).unwrap())),
        ("hop/rewrite_set", cost_per_op(|| perf::hop_rewrite_set(&inbound))),
        ("hop/stamp_rport", cost_per_op(|| perf::hop_stamp_received_rport(&inbound))),
        (
            "proxy_hop/invite",
            cost_per_op(|| perf::hop_minimal(&perf::request(&parser, perf::INVITE))),
        ),
        (
            "proxy_hop/invite_sdp",
            cost_per_op(|| perf::hop_minimal(&perf::request(&parser, &invite_sdp))),
        ),
        ("build/blank_draft", cost_per_op(|| blank.build())),
        (
            "build/invite_sdp",
            cost_per_op(|| generate_out_of_dialog_request(OutOfDialogMethod::Invite, &invite_opts)),
        ),
        (
            "build/bye",
            cost_per_op(|| generate_in_dialog_request(InDialogMethod::Bye, &dialog, &bye_opts)),
        ),
        (
            "build/response_200",
            cost_per_op(|| generate_response(&parsed_invite, 200, "OK", &response_opts)),
        ),
    ];

    println!("\nsip-message allocation budget ({ITERS} ops per case)");
    println!(
        "{:<22} {:>12} {:>12} {:>12} {:>12}",
        "case", "allocs/msg", "budget", "bytes/msg", "budget"
    );
    for (case, cost) in &measured {
        let b = budget(case);
        println!(
            "{case:<22} {:>12} {:>12} {:>12} {:>12}",
            cost.allocs, b.allocs, cost.bytes, b.bytes
        );
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
