//! The registry of compiled Callflow shapes.

use std::collections::BTreeMap;

use crate::shape::CallflowShape;

pub mod basic_call;

pub use basic_call::BasicCall;

/// The in-process Callflow-shape registry (ADR-0018): every compiled shape,
/// keyed by its stable id. Manual registration — adding a shape is a deliberate
/// act (extend here + publish its anchors).
pub fn registry() -> BTreeMap<String, Box<dyn CallflowShape>> {
    let shapes: Vec<Box<dyn CallflowShape>> = vec![Box::new(BasicCall)];
    shapes.into_iter().map(|s| (s.id().to_string(), s)).collect()
}
