//! The proxy's per-datagram path (ADR-0051 §5: the recv loop every call
//! shares): one proxy core over the simulated fabric per case, the input
//! sent twice from the peer of the seed it names, framed by that seed's
//! preamble and follow-up. The loop keeps turning and the parse guard
//! catches nothing.

#![no_main]

use std::sync::OnceLock;

use fuzz_lanes::input::proxy_case;
use fuzz_lanes::on_lane_stack;
use fuzz_lanes::proxy_lane::{rig, seeds};
use libfuzzer_sys::fuzz_target;

static SEEDS: OnceLock<Vec<seeds::Seed>> = OnceLock::new();

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    on_lane_stack(move || {
        let seeds = SEEDS.get_or_init(seeds::all);
        let (seed, datagram) = proxy_case(&data);
        let seed = &seeds[usize::from(seed) % seeds.len()];
        let outcome = rig::case(&seed.before, seed.from, datagram, &seed.after);
        assert_eq!(outcome.failure, None, "{}", seed.name);
    });
});
