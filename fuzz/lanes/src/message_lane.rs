//! The message layer's no-panic lane: every entry point that takes bytes
//! ([`probe`]) and its synthetic seeds ([`seeds`]).

#[path = "../../../crates/sip-message/tests/it/no_panic_lane/break_rule.rs"]
mod break_rule;
#[path = "../../../crates/sip-message/tests/it/no_panic_lane/echo.rs"]
mod echo;
#[path = "../../../crates/sip-message/tests/it/no_panic_lane/probe.rs"]
pub mod probe;
#[path = "../../../crates/sip-message/tests/it/no_panic_lane/readable.rs"]
mod readable;
#[path = "../../../crates/sip-message/tests/it/no_panic_lane/seeds.rs"]
pub mod seeds;
#[path = "../../../crates/sip-message/tests/it/no_panic_lane/typed_read.rs"]
pub(crate) mod typed_read;
