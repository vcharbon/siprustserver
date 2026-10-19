//! The no-panic property lane of the replicated call body (ADR-0051 §8):
//! whatever bytes a peer replicates, the call decode returns a call or
//! refuses them; it never panics and never exhausts the stack.
//!
//! The seeds are snapshots of calls the B2BUA ran ([`seeds`]). On threads
//! with the lane's small stack:
//!
//! - [`every_seed_decodes_or_refuses`] (default lane): every seed decodes to
//!   its call, and the seed cut by one byte, emptied, or nested past the
//!   decode's bound inside its service JSON is refused;
//! - [`structured_corpus_never_panics`] (slow lane): every deterministic
//!   MessagePack mutation of every seed ([`wire_mutations::msgpack::families`]);
//! - [`random_mutations_never_panic`] (slow lane): proptest over stacked
//!   random mutations, a fixed case count, failures persisted under
//!   `proptest-regressions/` beside this file.
//!
//! `just test-slow` runs the slow lane in the test profile, so overflow
//! checks and debug assertions are on.

use std::fmt::Write as _;

use call::{CallBodyCodec, MsgpackCodec};
use proptest::collection::vec;
use wire_mutations::lane::{on_lane_stack, runner, LANE_STACK};
use wire_mutations::msgpack::{self, Mutation, NEST_DEPTHS};

mod seeds;

/// Cases of the proptest property.
const CASES: u32 = 1024;

/// Threads the corpus is spread over, each on the lane's stack.
const WORKERS: usize = 4;

/// Decode `bytes`; a panic propagates and fails the lane.
fn decodes(bytes: &[u8]) -> bool {
    MsgpackCodec::new().decode(bytes).is_ok()
}

/// The offset just past the service JSON's key in `bytes`: where its value
/// starts.
fn service_value_at(bytes: &[u8]) -> usize {
    let key = b"lane-service";
    let needle: Vec<u8> =
        std::iter::once(0xa0 | key.len() as u8).chain(key.iter().copied()).collect();
    let at = bytes.windows(needle.len()).position(|w| w == needle).expect("the service key");
    at + needle.len()
}

/// Every seed decodes to its call; cut, emptied, or nested past the bound
/// at the untyped service JSON, it is refused.
#[test]
fn every_seed_decodes_or_refuses() {
    let seeds = seeds::all();
    on_lane_stack(move || {
        let codec = MsgpackCodec::new();
        for seed in &seeds {
            let decoded =
                codec.decode(&seed.bytes).unwrap_or_else(|e| panic!("{}: {e}", seed.name));
            assert_eq!(decoded, seed.call, "{} decodes to its call", seed.name);
            let cut = &seed.bytes[..seed.bytes.len() - 1];
            assert!(!decodes(cut), "{} cut by one byte is refused", seed.name);
        }
        assert!(!decodes(&[]), "an empty body is refused");
        let json = seeds.iter().find(|s| s.name.ends_with("+service-json")).expect("a JSON seed");
        let at = service_value_at(&json.bytes);
        let mut shallow = json.bytes.clone();
        shallow.splice(at..at, [0x91; 8]);
        assert!(decodes(&shallow), "the service JSON nested 8 deeper decodes");
        for (depth, &deep) in NEST_DEPTHS.iter().enumerate() {
            assert!(deep >= call::codec::MAX_DEPTH - 1);
            let nested = msgpack::apply(&json.bytes, &Mutation::Nest { at, depth });
            assert!(!decodes(&nested), "nested {deep} deep is refused");
        }
    });
}

/// Every structured mutation of every seed decodes or is refused.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn structured_corpus_never_panics() {
    let seeds = seeds::all();
    let jobs: Vec<(usize, &'static str, Mutation)> = seeds
        .iter()
        .enumerate()
        .flat_map(|(i, s)| {
            msgpack::families(&s.bytes)
                .into_iter()
                .flat_map(move |(family, ms)| ms.into_iter().map(move |m| (i, family, m)))
        })
        .collect();
    let (seeds, jobs) = (&seeds, &jobs);
    let failed: Vec<String> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..WORKERS)
            .map(|w| {
                std::thread::Builder::new()
                    .name(format!("snapshot-lane-{w}"))
                    .stack_size(LANE_STACK)
                    .spawn_scoped(scope, move || {
                        let mut failed = Vec::new();
                        for (i, family, m) in jobs.iter().skip(w).step_by(WORKERS) {
                            let seed = &seeds[*i];
                            let input = msgpack::apply(&seed.bytes, m);
                            let decoded = std::panic::catch_unwind(|| decodes(&input));
                            if decoded.is_err() {
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
    assert!(report.is_empty(), "the decode panicked:\n{report}");
}

/// Up to four stacked mutations of any seed decode or are refused.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn random_mutations_never_panic() {
    let seeds = seeds::all();
    on_lane_stack(move || {
        let strategy = (0..seeds.len(), vec(msgpack::mutation(), 1..=4));
        let mut runner = runner("tests/it/snapshot_lane/proptest-regressions/bodies.txt", CASES);
        let result = runner.run(&strategy, |(i, mutations)| {
            let input =
                mutations.iter().fold(seeds[i].bytes.clone(), |acc, m| msgpack::apply(&acc, m));
            decodes(&input);
            Ok(())
        });
        if let Err(e) = result {
            panic!("{e}");
        }
    });
}
