//! The proxy's per-datagram lane: one proxy core over the simulated fabric
//! per case, on any of its variants ([`rig`]), the seeds recorded off it
//! ([`seeds`]), the health probe reading a worker's answer ([`probe`]) and
//! the load report's value codec ([`report`]).

#[path = "../../../crates/sip-proxy/tests/it/no_panic_lane/probe.rs"]
pub mod probe;
#[path = "../../../crates/sip-proxy/tests/it/no_panic_lane/report.rs"]
pub mod report;
#[path = "../../../crates/sip-proxy/tests/it/no_panic_lane/rig.rs"]
pub mod rig;
#[path = "../../../crates/sip-proxy/tests/it/no_panic_lane/seeds.rs"]
pub mod seeds;
