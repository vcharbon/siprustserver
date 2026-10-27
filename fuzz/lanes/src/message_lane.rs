//! The message layer's no-panic lane: every entry point that takes bytes
//! ([`probe`]) and its synthetic seeds ([`seeds`]).

#[path = "../../../crates/sip-message/tests/it/no_panic_lane/echo.rs"]
mod echo;
#[path = "../../../crates/sip-message/tests/it/no_panic_lane/probe.rs"]
pub mod probe;
#[path = "../../../crates/sip-message/tests/it/no_panic_lane/seeds.rs"]
pub mod seeds;
