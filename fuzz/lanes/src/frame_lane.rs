//! The replication frame codec's lane: the stream reader and decoder over a
//! peer's bytes ([`case`]) and one stream per frame shape ([`seeds`]).

#[path = "../../../crates/repl-net/tests/it/no_panic_lane/case.rs"]
pub mod case;
#[path = "../../../crates/repl-net/tests/it/no_panic_lane/seeds.rs"]
pub mod seeds;
