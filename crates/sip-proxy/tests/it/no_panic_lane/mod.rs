//! The no-panic property lane of the proxy's per-datagram path (ADR-0051
//! §8): whatever datagram a caller, a worker or a callee sends, the recv loop
//! every call shares keeps turning — its Via, cookie and branch decoding and
//! its stateless forward decision included — and the parser it calls never
//! panics.
//!
//! A case ([`rig::case`]) runs one proxy core over the simulated fabric with
//! the load balancer the runner wires, on a paused clock: the seed's
//! unmutated preamble, the mutated datagram twice from the seed's peer, the
//! seed's follow-up, and the clock past the proxy's sweeps. A case fails
//! when the recv loop has ended or the parse guard counted a panic. The
//! seeds ([`seeds::all`]) are recorded off a run of the same proxy, so they
//! carry its own Via branch, cookie and Record-Route.
//!
//! Two slow-lane tests (`just test-slow` runs them in the test profile, so
//! overflow checks and debug assertions are on), on threads with the lane's
//! small stack:
//!
//! - [`structured_corpus_never_panics`]: every deterministic structured
//!   mutation of every seed ([`wire_mutations::mutate::families`]);
//! - [`random_mutations_never_panic`]: proptest over stacked random
//!   mutations, a fixed case count, failures persisted under
//!   `proptest-regressions/` beside this file.

use std::fmt::Write as _;

use proptest::collection::vec;
use wire_mutations::lane::{on_lane_stack, runner, LANE_STACK};
use wire_mutations::{mutate, Mutation};

mod rig;
mod seeds;

/// Cases of the proptest property.
const CASES: u32 = 1024;

/// Threads the corpus is spread over, each on the lane's stack.
const WORKERS: usize = 4;

/// Every seed, unmutated, is forwarded: the lane drives the proxy's routing
/// and relay paths, not only its parse refusals.
#[test]
fn every_seed_is_forwarded() {
    on_lane_stack(|| {
        for seed in seeds::all() {
            let outcome = rig::case(&seed.before, seed.from, &seed.bytes, &seed.after);
            assert_eq!(outcome.failure, None, "{}", seed.name);
            assert!(outcome.forwarded > 0, "{} was forwarded nowhere: {outcome:?}", seed.name);
        }
    });
}

/// Every structured mutation of every seed leaves the recv loop turning.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn structured_corpus_never_panics() {
    let seeds = seeds::all();
    let jobs: Vec<(usize, &'static str, Mutation)> = seeds
        .iter()
        .enumerate()
        .flat_map(|(i, s)| {
            mutate::families(&s.bytes)
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
                            let input = mutate::apply(&seed.bytes, m);
                            let outcome = rig::case(&seed.before, seed.from, &input, &seed.after);
                            if let Some(f) = outcome.failure {
                                failed.push(format!("{} {family} {m:?}: {f:?}", seed.name));
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
    assert!(report.is_empty(), "the recv loop did not survive:\n{report}");
}

/// Up to four stacked mutations of any seed leave the recv loop turning.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn random_mutations_never_panic() {
    on_lane_stack(|| {
        let seeds = seeds::all();
        let strategy = (0..seeds.len(), vec(mutate::mutation(), 1..=4));
        let mut runner = runner("tests/it/no_panic_lane/proptest-regressions/datagrams.txt", CASES);
        let result = runner.run(&strategy, |(i, mutations)| {
            let seed = &seeds[i];
            let input = mutations.iter().fold(seed.bytes.clone(), |acc, m| mutate::apply(&acc, m));
            match rig::case(&seed.before, seed.from, &input, &seed.after).failure {
                None => Ok(()),
                Some(f) => Err(proptest::test_runner::TestCaseError::fail(format!("{f:?}"))),
            }
        });
        if let Err(e) = result {
            panic!("{e}");
        }
    });
}
