//! The no-panic property lane of the transaction layer (ADR-0051 §8):
//! whatever datagram a peer sends, the owner loop every transaction shares
//! keeps turning and the parser it calls never panics.
//!
//! A case ([`drive::case`]) runs one layer over the simulated fabric, the
//! B2BUA worker's configuration, on a paused clock: the lane opens an INVITE
//! and a BYE client transaction toward the peer, the peer sends a seed's
//! unmutated preamble, the mutated datagram twice (a copy and its
//! retransmission) and the seed's follow-up, the lane's TU answers every
//! request it is handed, and the clock runs past every timer a transaction
//! can arm. A case fails when the owner task has ended or the parse guard
//! counted a panic.
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

mod drive;
mod seeds;

/// Cases of the proptest property.
const CASES: u32 = 1024;

/// Threads the corpus is spread over, each on the lane's stack.
const WORKERS: usize = 4;

/// Every seed, unmutated, reaches what it is for, after its preamble: a
/// request reaches the TU, a response matches its client transaction, and
/// the 100 the layer absorbs stops the client INVITE's retransmissions. The
/// lane drives the layer's request and response paths, not only its parse
/// refusals.
#[test]
fn every_seed_reaches_the_tu() {
    on_lane_stack(|| {
        for seed in seeds::all() {
            let outcome = drive::outcome(&seed, &seed.bytes);
            assert_eq!(outcome.failure, None, "{}", seed.name);
            if seed.name == "resp/100-invite" {
                assert_eq!(outcome.invite_copies, 1, "the 100 stops Timer A: {outcome:?}");
            } else if seed.name.starts_with("resp/") {
                assert!(outcome.matched > 0, "{} matched no client txn: {outcome:?}", seed.name);
            } else {
                assert!(outcome.handed_up > 0, "{} reached no TU: {outcome:?}", seed.name);
            }
            // The check sees the seed, not its preamble: a datagram the
            // parser refuses in its place reaches nothing.
            let refused = drive::outcome(&seed, b"not a SIP message");
            assert_eq!((refused.handed_up, refused.matched), (0, 0), "{}", seed.name);
            assert!(refused.invite_copies > 1, "{}: Timer A runs: {refused:?}", seed.name);
        }
    });
}

/// Every structured mutation of every seed leaves the owner loop turning.
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
                            if let Some(f) = drive::case(seed, &input) {
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
    assert!(report.is_empty(), "the owner loop did not survive:\n{report}");
}

/// Up to four stacked mutations of any seed leave the owner loop turning.
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
            match drive::case(seed, &input) {
                None => Ok(()),
                Some(f) => Err(proptest::test_runner::TestCaseError::fail(format!("{f:?}"))),
            }
        });
        if let Err(e) = result {
            panic!("{e}");
        }
    });
}
