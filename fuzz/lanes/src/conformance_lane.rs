//! The conformance suite's grammar generator ([`generated::must_accept`]),
//! compiled in from the crate's own test tree, so the must-accept target reads
//! exactly the values and messages the suite reads.

#[path = "../../../crates/sip-message/tests/it/conformance/abnf.rs"]
pub mod abnf;
#[allow(dead_code, unused_imports)]
#[path = "../../../crates/sip-message/tests/it/conformance/generated.rs"]
pub mod generated;
#[allow(dead_code)]
#[path = "../../../crates/sip-message/tests/it/conformance/message.rs"]
mod message;
#[path = "../../../crates/sip-message/tests/it/conformance/parameters.rs"]
mod parameters;
#[allow(dead_code)]
#[path = "../../../crates/sip-message/tests/it/conformance/ratchet.rs"]
mod ratchet;
#[path = "../../../crates/sip-message/tests/it/conformance/stated.rs"]
mod stated;
