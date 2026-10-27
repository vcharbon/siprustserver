//! The proxy's per-datagram path on the rig's variants the peer drives past
//! the plain one: its self-gate closed, no worker alive, one worker down,
//! two faces, names that resolve to nothing. The input is sent twice from
//! the peer of the variant's seed it names, framed by that seed's preamble
//! and follow-up. The loop keeps turning and the parse guard catches
//! nothing.

#![no_main]

use std::sync::OnceLock;

use fuzz_lanes::input::variant_case;
use fuzz_lanes::on_lane_stack;
use fuzz_lanes::proxy_lane::rig::{self, Variant};
use fuzz_lanes::proxy_lane::seeds;
use libfuzzer_sys::fuzz_target;

static SEEDS: OnceLock<Vec<Vec<seeds::Seed>>> = OnceLock::new();

fuzz_target!(|data: &[u8]| {
    let data = data.to_vec();
    on_lane_stack(move || {
        let seeds =
            SEEDS.get_or_init(|| Variant::DATAGRAM.iter().map(|v| seeds::all_on(*v)).collect());
        let (variant, seed, datagram) = variant_case(&data);
        let v = usize::from(variant) % Variant::DATAGRAM.len();
        let seed = &seeds[v][usize::from(seed) % seeds[v].len()];
        let variant = Variant::DATAGRAM[v];
        let outcome = rig::case_on(variant, &seed.before, seed.from, datagram, &seed.after);
        assert_eq!(outcome.failure, None, "{variant:?} {}", seed.name);
    });
});
