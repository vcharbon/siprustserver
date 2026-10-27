//! The proxy's per-datagram lane: one proxy core over the simulated fabric
//! per case ([`rig`]) and the seeds recorded off it ([`seeds`]).

#[path = "../../../crates/sip-proxy/tests/it/no_panic_lane/rig.rs"]
pub mod rig;
#[path = "../../../crates/sip-proxy/tests/it/no_panic_lane/seeds.rs"]
pub mod seeds;
