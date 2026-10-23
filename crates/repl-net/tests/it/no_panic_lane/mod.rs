//! The no-panic property lane of the replication frame codec (ADR-0051 §8):
//! whatever bytes a peer's stream carries, the reader that delimits it and
//! the decoder of each frame return a frame or a typed error, never a panic.
//!
//! A case ([`case`]) runs the stream reader the real transport runs: it pops
//! every length-prefixed payload the bytes hold and decodes each, and it
//! decodes the bytes after the first prefix as one frame, so a mutation
//! reaches the decoder whether or not the framing still holds.
//!
//! Two slow-lane tests (`just test-slow` runs them in the test profile, so
//! overflow checks and debug assertions are on), on threads with the lane's
//! small stack:
//!
//! - [`structured_corpus_never_panics`]: every deterministic structured
//!   mutation of every seed ([`wire_mutations::binary::families`]);
//! - [`random_mutations_never_panic`]: proptest over stacked random
//!   mutations, a fixed case count, failures persisted under
//!   `proptest-regressions/` beside this file.

use std::fmt::Write as _;
use std::panic::{catch_unwind, AssertUnwindSafe};

use proptest::collection::vec;
use repl_net::{decode_frame, try_read_framed, Frame};
use wire_mutations::binary::{self, Mutation};
use wire_mutations::lane::{on_lane_stack, runner, LANE_STACK};

mod seeds;

/// Cases of the proptest property.
const CASES: u32 = 4096;

/// Threads the corpus is spread over, each on the lane's stack.
const WORKERS: usize = 4;

/// The frames the stream reader decodes off `wire`, in order, and the frame
/// the bytes after the first prefix decode to, if any.
fn case(wire: &[u8]) -> (Vec<Frame>, Option<Frame>) {
    let mut buf = wire.to_vec();
    let mut frames = Vec::new();
    while let Ok(Some(payload)) = try_read_framed(&mut buf) {
        match decode_frame(&payload) {
            Ok(frame) => frames.push(frame),
            Err(_) => break,
        }
    }
    let bare = decode_frame(wire.get(4..).unwrap_or_default()).ok();
    (frames, bare)
}

/// Every seed, unmutated, decodes to the frames it was encoded from: the lane
/// drives the decoder's paths to a frame, not only its refusals.
#[test]
fn every_seed_decodes_to_its_frames() {
    on_lane_stack(|| {
        for seed in seeds::all() {
            let (frames, bare) = case(&seed.wire);
            assert_eq!(frames, seed.frames, "{}", seed.name);
            assert_eq!(bare.as_ref(), seed.frames.first(), "{}", seed.name);
            let (cut, _) = case(seed.wire.get(..seed.wire.len() - 1).unwrap_or_default());
            assert_eq!(cut.len(), seed.frames.len() - 1, "{}: a cut frame is not read", seed.name);
        }
    });
}

/// Every structured mutation of every seed returns a frame or an error.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn structured_corpus_never_panics() {
    let seeds = seeds::all();
    let jobs: Vec<(usize, &'static str, Mutation)> = seeds
        .iter()
        .enumerate()
        .flat_map(|(i, s)| {
            binary::families(&s.wire)
                .into_iter()
                .flat_map(move |(family, ms)| ms.into_iter().map(move |m| (i, family, m)))
        })
        .collect();
    let (seeds, jobs) = (&seeds, &jobs);
    let failed: Vec<String> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..WORKERS)
            .map(|w| {
                std::thread::Builder::new()
                    .name(format!("no-panic-lane-{w}"))
                    .stack_size(LANE_STACK)
                    .spawn_scoped(scope, move || {
                        let mut failed = Vec::new();
                        for (i, family, m) in jobs.iter().skip(w).step_by(WORKERS) {
                            let seed = &seeds[*i];
                            let input = binary::apply(&seed.wire, m);
                            if catch_unwind(AssertUnwindSafe(|| case(&input))).is_err() {
                                failed.push(format!("{} {family} {m:?}", seed.name));
                            }
                        }
                        failed
                    })
                    .expect("spawn a lane worker")
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().expect("lane worker")).collect()
    });
    let mut report = String::new();
    for line in &failed {
        let _ = writeln!(report, "{line}");
    }
    assert!(report.is_empty(), "the codec panicked:\n{report}");
}

/// Up to four stacked mutations of any seed return a frame or an error.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn random_mutations_never_panic() {
    on_lane_stack(|| {
        let seeds = seeds::all();
        let strategy = (0..seeds.len(), vec(binary::mutation(), 1..=4));
        let mut runner = runner("tests/it/no_panic_lane/proptest-regressions/frames.txt", CASES);
        let result = runner.run(&strategy, |(i, mutations)| {
            let input =
                mutations.iter().fold(seeds[i].wire.clone(), |acc, m| binary::apply(&acc, m));
            case(&input);
            Ok(())
        });
        if let Err(e) = result {
            panic!("{e}");
        }
    });
}
