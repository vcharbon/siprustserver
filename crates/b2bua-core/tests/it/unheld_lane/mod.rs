//! The no-panic property lane of the answer to a response for a call this
//! node does not hold (ADR-0055 §4, §12): whatever a peer puts in a final
//! that carries our signed via-extension — its Contact, Record-Route, SDP
//! and the extension itself — the answer is built from it without a panic,
//! and a verified final is answered as it is owed ([`case::case`]).
//!
//! - [`every_seed_draws_what_it_is_owed`] (default lane): each seed,
//!   unmutated;
//! - [`structured_corpus_never_panics`] (slow lane): every deterministic
//!   structured mutation of every seed;
//! - [`random_mutations_never_panic`] (slow lane): proptest over stacked
//!   random mutations, failures persisted under `proptest-regressions/`
//!   beside this file.

use proptest::collection::vec;
use wire_mutations::lane::{on_lane_stack, runner};
use wire_mutations::mutate;

use b2bua_core::effects::OutboundBody;

pub(crate) mod case;
pub(crate) mod seeds;

/// Cases of the proptest property.
const CASES: u32 = 2048;

/// Each seed draws the requests it is owed: a 2xx its ACK and BYE (the ACK
/// answering an offer rejecting every stream), a non-2xx its ACK, a
/// provisional or a forged signature nothing.
#[test]
fn every_seed_draws_what_it_is_owed() {
    on_lane_stack(|| {
        for seed in seeds::all() {
            let sent: Vec<(String, String)> = case::case(&seed.bytes)
                .iter()
                .filter_map(|e| match &e.body {
                    OutboundBody::Request(r) => Some((
                        r.method().as_str().to_string(),
                        String::from_utf8_lossy(r.body()).into_owned(),
                    )),
                    _ => None,
                })
                .collect();
            let methods: Vec<&str> = sent.iter().map(|(m, _)| m.as_str()).collect();
            let owed: &[&str] = match seed.name {
                "2xx" | "2xx-offer" => &["ACK", "BYE"],
                "non-2xx" => &["ACK"],
                _ => &[],
            };
            assert_eq!(methods, owed, "{}", seed.name);
            if seed.name == "2xx-offer" {
                assert!(sent[0].1.contains("m=audio 0 "), "every stream rejected: {}", sent[0].1);
            }
        }
    });
}

/// Every structured mutation of every seed is answered without a panic.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn structured_corpus_never_panics() {
    on_lane_stack(|| {
        for seed in seeds::all() {
            for (family, mutations) in mutate::families(&seed.bytes) {
                for m in mutations {
                    let input = mutate::apply(&seed.bytes, &m);
                    let answered = std::panic::catch_unwind(|| case::case(&input).len());
                    assert!(answered.is_ok(), "{} {family} {m:?} panicked", seed.name);
                }
            }
        }
    });
}

/// Up to four stacked mutations of any seed are answered without a panic.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn random_mutations_never_panic() {
    on_lane_stack(|| {
        let seeds = seeds::all();
        let strategy = (0..seeds.len(), vec(mutate::mutation(), 1..=4));
        let mut runner = runner("tests/it/unheld_lane/proptest-regressions/responses.txt", CASES);
        let result = runner.run(&strategy, |(i, mutations)| {
            let seed = &seeds[i].bytes;
            let input = mutations.iter().fold(seed.clone(), |acc, m| mutate::apply(&acc, m));
            case::case(&input);
            Ok(())
        });
        if let Err(e) = result {
            panic!("{e}");
        }
    });
}
