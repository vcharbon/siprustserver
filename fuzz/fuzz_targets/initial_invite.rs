//! The caller's INVITE (ADR-0051 §5: every caller reaches it before any
//! dialog exists), folded through a decision's route as the shell's initial
//! turn folds it: plain, with stated headers, with parts attached beside the
//! caller's body, with the body replaced, toward a refused destination. The
//! call is refused, or its b-leg INVITE reads back and refreezes to itself;
//! a script of the plain call then runs over the call it dialled, twice,
//! and the runs agree.

#![no_main]

use std::sync::OnceLock;

use fuzz_lanes::input::invite_case;
use fuzz_lanes::on_lane_stack;
use fuzz_lanes::turn_lane::drive::{self, Setup};
use fuzz_lanes::turn_lane::{invite, scripts};
use libfuzzer_sys::fuzz_target;

static DECISIONS: OnceLock<Vec<invite::Decision>> = OnceLock::new();

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    on_lane_stack(move || {
        let decisions = DECISIONS.get_or_init(invite::decisions);
        let (decision, script, text) = invite_case(&data);
        let decision = &decisions[usize::from(decision) % decisions.len()];
        let Some(dialled) = invite::dial_checked(text, decision) else { return };
        let plain: Vec<scripts::Script> =
            scripts::scripts().into_iter().filter(|s| s.setup == Setup::Plain).collect();
        let script = &plain[usize::from(script) % plain.len()];
        let first = drive::run(&dialled, &script.steps, usize::MAX, &[]);
        let again = drive::run(&dialled, &script.steps, usize::MAX, &[]);
        assert_eq!(first.turns, again.turns, "a turn is a function of its inputs");
    });
});
