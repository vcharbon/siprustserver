//! A valid message must parse: a value of one header's grammar or of a start
//! line (the conformance suite's generator, RFC 3261 §25.1 and the extensions'
//! grammars), its derivation steered by the input, in a whole message read
//! through the production path and then its header's typed reader. Every
//! input steers to a value the grammar admits, so a refusal is a false one,
//! unless a stated rule refuses that form (the suite's `stated.rs`).

#![no_main]

use std::sync::OnceLock;

use fuzz_lanes::conformance_lane::abnf::Grammar;
use fuzz_lanes::conformance_lane::generated;
use fuzz_lanes::input::grammar_case;
use fuzz_lanes::on_lane_stack;
use libfuzzer_sys::fuzz_target;

static GRAMMAR: OnceLock<Grammar> = OnceLock::new();

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    on_lane_stack(move || {
        let grammar = GRAMMAR.get_or_init(generated::grammar);
        let (target, steer) = grammar_case(&data);
        if let Err(refusal) = generated::must_accept(grammar, target, steer) {
            panic!("a valid value is refused: {refusal}");
        }
    });
});
