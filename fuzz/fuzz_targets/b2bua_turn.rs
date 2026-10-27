//! A call's turns over any SIP text a peer sends into it (ADR-0051 §5: a
//! panic in a turn tears the call down). A script of the turn lane runs over
//! the dialled call, the SIP text of one of its steps replaced by the input;
//! the texts the scripts carry seed the corpus, so mutations start from
//! messages that match the call's dialogs. A turn is a function of its
//! inputs: two runs of a case agree.

#![no_main]

use fuzz_lanes::input::turn_case;
use fuzz_lanes::on_lane_stack;
use fuzz_lanes::turn_lane::{drive, scripts};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    on_lane_stack(move || {
        let case = turn_case(&data);
        let scripts = scripts::scripts();
        let (_, script) = &scripts[usize::from(case.script) % scripts.len()];
        let step = usize::from(case.step) % script.len();
        let dialled = drive::dialled();
        let first = drive::run_replacing(&dialled, script, step, case.text);
        let again = drive::run_replacing(&dialled, script, step, case.text);
        assert_eq!(first.turns, again.turns, "a turn is a function of its inputs");
    });
});
