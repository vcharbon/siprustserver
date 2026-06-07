//! The rule-engine core types (declarative `Match`, `RuleContext`, the
//! `RuleAction` vocabulary). These moved to the public Rule SDK (`b2bua-sdk`,
//! ADR-0016 slice 6) so a service crate is authored against them without a
//! dependency on `b2bua`; this module re-exports them so in-tree
//! `crate::rules::model` paths are unchanged.

pub use b2bua_sdk::model::*;
