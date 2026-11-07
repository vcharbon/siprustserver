//! The conformance suite: every valid SIP message parses through the production
//! path, and what the RFC forbids is refused.
//!
//! Each family of rows is a table run in the default lane at a count that keeps it
//! under a second: `abnf` (generated values of every header's grammar), `frozen`
//! (the vendored corpus), `rfc` (the RFCs' examples and messages), `must` (their
//! MUST statements on message syntax), `refuse` and `recase` (one rule broken by
//! construction, a repeat among them), `body` and `sdp` (what `sdp()` and the description readers
//! return), `exempt` (the generic exemption set) and `readable` (accepted means
//! readable). A row that fails sits on `ratchet.txt` with the area that owns it.

mod abnf;
mod bodies;
pub(crate) mod break_rule;
mod exemption_mode;
pub(crate) mod generated;
pub(crate) mod message;
mod must_statements;
mod parameters;
pub(crate) mod ratchet;
mod readable_rows;
mod repeats;
pub(crate) mod rfc_examples;
pub(crate) mod stated;
