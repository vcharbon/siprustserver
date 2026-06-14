//! The registry of compiled Callflow shapes.

use std::collections::BTreeMap;

use crate::shape::CallflowShape;

pub mod basic_call;
pub mod basic_call_media;
pub mod rerouting;
pub mod rerouting_prack;
pub mod transfer_refer_media;

pub use basic_call::BasicCall;
pub use basic_call_media::BasicCallMedia;
pub use rerouting::Rerouting;
pub use rerouting_prack::ReroutingPrack;
pub use transfer_refer_media::TransferReferMedia;

/// The in-process Callflow-shape registry (ADR-0018): every compiled shape,
/// keyed by its stable id. Manual registration — adding a shape is a deliberate
/// act (extend here + publish its anchors).
pub fn registry() -> BTreeMap<String, Box<dyn CallflowShape>> {
    let shapes: Vec<Box<dyn CallflowShape>> = vec![
        Box::new(BasicCall),
        Box::new(BasicCallMedia),
        Box::new(Rerouting),
        Box::new(ReroutingPrack),
        Box::new(TransferReferMedia),
    ];
    shapes.into_iter().map(|s| (s.id().to_string(), s)).collect()
}
