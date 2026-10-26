// Own binary (ADR-0030 X2): installs the global allocation counter.
//! Scaling shape of the parse, typed-read, draft and generator paths.
//!
//! ADR-0051 §4 holds parse work linear in the input size. Each case here
//! runs one operation over inputs that grow along one axis (header lines, Via
//! or Route entries, body bytes) and counts what it costs at each point: the
//! allocation events and requested bytes (`alloc-counter`) and the
//! instructions the thread retires in user space (`insn-counter`). Both are
//! deterministic, so the verdict is a growth factor, never a time: an
//! operation whose cost more than keeps pace with its input fails.
//!
//! The shape table of every case: `cargo nextest run --workspace -E
//! 'package(=sip-message) & binary(scaling)' --run-ignored only --no-capture`.

mod ops;
mod wire;

use alloc_counter::{measure, CountingAlloc};
use insn_counter::InsnCounter;

/// Counts every allocation of the process. Each test of this binary measures
/// one region at a time on its own thread, and nextest runs each test in a
/// process of its own.
#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// Operations per allocation-measured region.
const REPS: usize = 8;

/// What one operation costs at one axis point.
#[derive(Debug, Clone, Copy)]
struct Cost {
    allocs: usize,
    bytes: usize,
    insns: u64,
}

fn cost(counter: &mut InsnCounter, op: &mut ops::Op) -> Cost {
    op();
    let ((), alloc) = measure(|| {
        for _ in 0..REPS {
            op();
        }
    });
    let alloc = alloc.per(REPS);
    // The least of three runs: a page fault or a lazily bound symbol can only
    // add instructions.
    let insns = (0..3)
        .map(|_| counter.measure(&mut *op).expect("the counter reads").1)
        .min()
        .unwrap_or_default();
    Cost { allocs: alloc.allocs, bytes: alloc.bytes, insns }
}

/// How a case's cost may grow along its axis.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Growth {
    /// At most in proportion to the input.
    Linear,
    /// Not with the input at all.
    Constant,
}

/// One operation and the axis it is grown on.
struct Case {
    name: &'static str,
    axis: &'static str,
    op: fn(usize) -> ops::Op,
    /// The axis points of the shape table.
    table: &'static [usize],
    /// The two axis points the gate compares, and the growth it allows.
    gate: Option<(usize, usize, Growth)>,
}

const N: &[usize] = &[10, 50, 100];
const ENTRIES: &[usize] = &[1, 10, 50];
const KIB: &[usize] = &[0, 4, 32, 60];

const fn case(
    name: &'static str,
    axis: &'static str,
    op: fn(usize) -> ops::Op,
    table: &'static [usize],
    gate: Option<(usize, usize, Growth)>,
) -> Case {
    Case { name, axis, op, table, gate }
}

const LINEAR_LINES: Option<(usize, usize, Growth)> = Some((100, 400, Growth::Linear));
const LINEAR_ENTRIES: Option<(usize, usize, Growth)> = Some((50, 200, Growth::Linear));
const LINEAR_KIB: Option<(usize, usize, Growth)> = Some((16, 64, Growth::Linear));

const CASES: &[Case] = &[
    case("parse", "header lines", ops::parse_lines, N, LINEAR_LINES),
    case("parse", "Via lines", ops::parse_vias, ENTRIES, LINEAR_ENTRIES),
    case("parse", "Via entries, one line", ops::parse_vias_folded, ENTRIES, LINEAR_ENTRIES),
    case("parse", "Route lines", ops::parse_routes, ENTRIES, LINEAR_ENTRIES),
    case("parse", "Route entries, one line", ops::parse_routes_folded, ENTRIES, LINEAR_ENTRIES),
    case("parse", "Via params", ops::parse_via_params, &[10, 100, 400], LINEAR_ENTRIES),
    case("parse", "From URI params", ops::parse_from_uri_params, &[10, 100, 400], LINEAR_ENTRIES),
    case("parse", "SDP body KiB", ops::parse_sdp_body_kib, KIB, LINEAR_KIB),
    case("parse", "multipart body KiB", ops::parse_multipart_kib, KIB, LINEAR_KIB),
    case(
        "read absent header",
        "header lines",
        ops::read_absent,
        N,
        Some((100, 400, Growth::Constant)),
    ),
    case(
        "read single header",
        "header lines",
        ops::read_present_single,
        N,
        Some((100, 400, Growth::Constant)),
    ),
    case(
        "read single header",
        "lines of it",
        ops::read_single_repeated,
        ENTRIES,
        Some((50, 200, Growth::Constant)),
    ),
    case("read turn set", "header lines", ops::read_turn, N, LINEAR_LINES),
    case("read route_set", "Route lines", ops::read_route_set, ENTRIES, LINEAR_ENTRIES),
    case("read sdp()", "SDP body KiB", ops::read_sdp_kib, KIB, LINEAR_KIB),
    case("read sdp()", "multipart body KiB", ops::read_multipart_sdp_kib, KIB, LINEAR_KIB),
    case(
        "parse + sdp() x reads",
        "reads, multipart 32 KiB",
        ops::sdp_reads_multipart,
        &[1, 4, 8],
        Some((1, 8, Growth::Constant)),
    ),
    case(
        "parse + sdp() x 8",
        "multipart body KiB",
        ops::sdp_eight_reads_multipart_kib,
        KIB,
        LINEAR_KIB,
    ),
    case("proxy hop", "header lines", ops::hop_lines, N, LINEAR_LINES),
    case("proxy hop", "Via lines", ops::hop_vias, ENTRIES, LINEAR_ENTRIES),
    case("proxy hop", "Via entries, one line", ops::hop_vias_folded, ENTRIES, LINEAR_ENTRIES),
    case("hop popping own routes", "Route lines", ops::hop_pop_routes, ENTRIES, LINEAR_ENTRIES),
    case(
        "hop popping own routes",
        "Route entries, one line",
        ops::hop_pop_routes_folded,
        ENTRIES,
        LINEAR_ENTRIES,
    ),
    case(
        "hop popping own routes",
        "Route entries, 100 a line",
        ops::hop_pop_routes_datagram,
        &[100, 400, 1600],
        Some((400, 1600, Growth::Linear)),
    ),
    case("draft vias() identity", "Via lines", ops::draft_list_vias, ENTRIES, LINEAR_ENTRIES),
    case(
        "HeaderList push_front x n",
        "pushes",
        ops::header_list_push_front,
        ENTRIES,
        Some((200, 800, Growth::Linear)),
    ),
    case(
        "HeaderList pop_front x n",
        "entries",
        ops::header_list_pop_front,
        ENTRIES,
        Some((200, 800, Growth::Linear)),
    ),
    case("thaw + set + freeze", "header lines", ops::freeze_lines, N, LINEAR_LINES),
    case("thaw + set + freeze_bytes", "header lines", ops::freeze_bytes_lines, N, LINEAR_LINES),
    case("clone parsed request", "header lines", ops::clone_request_lines, N, LINEAR_LINES),
    case("thaw + set + freeze", "Via lines", ops::freeze_vias, ENTRIES, LINEAR_ENTRIES),
    case("blank draft + n typed + freeze", "typed lines", ops::build_typed, N, LINEAR_LINES),
    case("generate_response 200", "header lines", ops::response_lines, N, LINEAR_LINES),
    case("generate_response 200", "Via lines", ops::response_vias, ENTRIES, LINEAR_ENTRIES),
    case(
        "generate_response 200",
        "Record-Route lines",
        ops::response_record_routes,
        ENTRIES,
        LINEAR_ENTRIES,
    ),
    case("in-dialog BYE", "route set entries", ops::in_dialog_routes, ENTRIES, LINEAR_ENTRIES),
    case("relay mint (b-leg INVITE)", "header lines", ops::relay_mint_lines, N, LINEAR_LINES),
    case("parse_sdp_body", "SDP KiB, sections", ops::sdp_parse_sections_kib, KIB, LINEAR_KIB),
    case("parse_sdp_body", "SDP KiB, attributes", ops::sdp_parse_attributes_kib, KIB, LINEAR_KIB),
    case("extract_codec_profile", "SDP KiB, sections", ops::sdp_codec_profile_kib, KIB, LINEAR_KIB),
    case("answer_from_own", "SDP KiB, sections", ops::sdp_answer_sections_kib, KIB, LINEAR_KIB),
    case("answer_from_own", "SDP KiB, attributes", ops::sdp_answer_attributes_kib, KIB, LINEAR_KIB),
    case("multipart decompose", "body KiB", ops::multipart_decompose_kib, KIB, LINEAR_KIB),
];

/// Growth beyond the axis ratio a linear case may show, and the growth a
/// constant case may show at all: room for a fixed cost that is not quite
/// fixed (a Vec sized from the input), none for a term in the square.
const SLACK: f64 = 1.25;

fn ratio(a: f64, b: f64) -> f64 {
    if a == 0.0 {
        if b == 0.0 {
            1.0
        } else {
            f64::INFINITY
        }
    } else {
        b / a
    }
}

/// Why the cost from `small` to `large` breaks `growth`, if it does.
fn breach(
    case: &Case,
    small: (usize, Cost),
    large: (usize, Cost),
    growth: Growth,
) -> Option<String> {
    let axis = ratio(small.0 as f64, large.0 as f64);
    let allowed = match growth {
        Growth::Linear => axis * SLACK,
        Growth::Constant => SLACK,
    };
    let insns = ratio(small.1.insns as f64, large.1.insns as f64);
    // An allocation count is small and whole: one more allocation is noise,
    // not a shape.
    let allocs = ratio(small.1.allocs as f64 + 1.0, large.1.allocs as f64 + 1.0);
    (insns > allowed || allocs > allowed).then(|| {
        format!(
            "{} over {} ({:?}): {} -> {} grew instructions x{insns:.2}, allocations x{allocs:.2} \
             (allowed x{allowed:.2}); {:?} -> {:?}",
            case.name, case.axis, growth, small.0, large.0, small.1, large.1
        )
    })
}

/// Every gated case grows no faster than its stated shape.
#[test]
fn work_grows_with_the_input_as_each_path_states() {
    let mut counter = InsnCounter::open().expect(
        "a user-space instruction counter (perf_event_open, perf_event_paranoid <= 2) is the \
         work count this gate reads",
    );
    let mut breaches = Vec::new();
    for case in CASES {
        let Some((small, large, growth)) = case.gate else { continue };
        let at_small = cost(&mut counter, &mut (case.op)(small));
        let at_large = cost(&mut counter, &mut (case.op)(large));
        println!(
            "{:<32} {:<26} {small:>5} -> {large:<5} insns x{:.2} allocs {} -> {}",
            case.name,
            case.axis,
            ratio(at_small.insns as f64, at_large.insns as f64),
            at_small.allocs,
            at_large.allocs
        );
        breaches.extend(breach(case, (small, at_small), (large, at_large), growth));
    }
    assert!(breaches.is_empty(), "superlinear paths:\n{}", breaches.join("\n"));
}

/// The shape table: every case at the ticket's axis points, with the growth
/// factor of each step.
#[test]
#[ignore = "shape report: run on demand, prints the table"]
fn shape_table() {
    let mut counter = InsnCounter::open().expect("a user-space instruction counter");
    println!("| path | axis | size | allocs | bytes | instructions | allocs x | instructions x |");
    println!("|---|---|---:|---:|---:|---:|---:|---:|");
    for case in CASES {
        let mut previous: Option<(usize, Cost)> = None;
        let doubled = case.table.last().map(|last| (last * 2).max(1));
        for size in case.table.iter().copied().chain(doubled) {
            let at = cost(&mut counter, &mut (case.op)(size));
            let (allocs_x, insns_x) = match previous {
                Some((_, p)) => (
                    format!("{:.2}", ratio(p.allocs as f64, at.allocs as f64)),
                    format!("{:.2}", ratio(p.insns as f64, at.insns as f64)),
                ),
                None => (String::new(), String::new()),
            };
            println!(
                "| {} | {} | {size} | {} | {} | {} | {allocs_x} | {insns_x} |",
                case.name, case.axis, at.allocs, at.bytes, at.insns
            );
            previous = Some((size, at));
        }
    }
}
