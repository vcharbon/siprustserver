//! The **functional side** of the unified shape registry (ADR-0018): the
//! `!Send` [`CallflowShape`] bodies, ATTACHED by id to the descriptors declared
//! once in [`e2e_model::ShapeRegistry`] (see its module docs — one id space,
//! two run surfaces). [`registry`] is the shipped default; a third-party crate
//! composes its own via [`attach`] over an extended `ShapeRegistry` (its
//! descriptors registered first, its bodies attached after) — adding a shape is
//! a deliberate act on BOTH sides: declare the descriptor, then attach the body.

use std::collections::BTreeMap;

use e2e_model::{ShapeDescriptor, ShapeRegistry};
use e2e_model::shape::{AsShapeSpec, ShapeSpec};

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

/// One functional registry entry: the shape's ONE declaration (the descriptor —
/// id, anchors, required input, params schema, load attributes) plus its
/// attached `!Send` functional body.
pub struct ShapeEntry {
    pub descriptor: ShapeDescriptor,
    pub body: Box<dyn CallflowShape>,
}

/// Attach functional bodies to their descriptors **by id** — the open seam a
/// third-party crate uses with its own extended [`ShapeRegistry`]. Panics on an
/// id with no descriptor (declare the shape in the registry first — one id
/// space, never a body-only shape) and on a duplicate attachment.
pub fn attach(
    registry: &ShapeRegistry,
    bodies: impl IntoIterator<Item = (&'static str, Box<dyn CallflowShape>)>,
) -> BTreeMap<String, ShapeEntry> {
    let mut out = BTreeMap::new();
    for (id, body) in bodies {
        let descriptor = registry.get(id).cloned().unwrap_or_else(|| {
            panic!(
                "no ShapeDescriptor registered for functional body {id:?} — declare the shape \
                 in the ShapeRegistry first (one id space; known: {:?})",
                registry.ids()
            )
        });
        if out.insert(id.to_string(), ShapeEntry { descriptor, body }).is_some() {
            panic!("duplicate functional body attached for shape {id:?}");
        }
    }
    out
}

/// The shipped functional bodies, keyed by the descriptor id they attach to.
pub fn default_bodies() -> Vec<(&'static str, Box<dyn CallflowShape>)> {
    vec![
        ("basic-call", Box::new(BasicCall) as Box<dyn CallflowShape>),
        ("basic-call-media", Box::new(BasicCallMedia)),
        ("rerouting", Box::new(Rerouting)),
        // The first DUAL-BODY shape: the same descriptor also carries the load
        // body (`scenario_harness::actor::scenarios::ReroutingPrack`).
        ("rerouting_prack", Box::new(ReroutingPrack)),
        ("transfer-refer-media", Box::new(TransferReferMedia)),
    ]
}

/// The in-process functional shape registry: every shipped descriptor with a
/// functional body, keyed by its stable id — [`attach`] over
/// [`ShapeRegistry::with_defaults`].
pub fn registry() -> BTreeMap<String, ShapeEntry> {
    attach(&ShapeRegistry::with_defaults(), default_bodies())
}

/// The functional registry is a `ShapeCatalog` (via the blanket
/// `BTreeMap<String, impl AsShapeSpec>` impl): `validate_case` reads each
/// entry's DESCRIPTOR (anchors / required input) — the same metadata the load
/// surface and the pure-descriptor registry validate against.
impl AsShapeSpec for ShapeEntry {
    fn as_spec(&self) -> &dyn ShapeSpec {
        &self.descriptor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::InfraRuntime;
    use crate::model::Input;
    use async_trait::async_trait;

    /// The unified path serves ALL previous shapes plus the dual-body
    /// `rerouting_prack`: every id resolves to a descriptor + functional body,
    /// the descriptor is the `ShapeSpec` the validation reads, and the dual
    /// shape ALSO carries the load body on the same declaration.
    #[test]
    fn functional_registry_serves_all_shapes_via_the_unified_path() {
        let reg = registry();
        assert_eq!(
            reg.keys().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "basic-call",
                "basic-call-media",
                "rerouting",
                "rerouting_prack",
                "transfer-refer-media"
            ]
        );
        for (id, entry) in &reg {
            assert_eq!(&entry.descriptor.id, id, "descriptor id matches the key");
            assert!(
                !ShapeSpec::anchors(&entry.descriptor).is_empty(),
                "{id}: a functional shape publishes anchors"
            );
            assert!(!entry.body.agents().is_empty(), "{id}: the body declares its roster");
        }
        // Dual-body: the SAME descriptor the functional body attached to also
        // mints the load body (one declaration, two run surfaces).
        let dual = &reg["rerouting_prack"].descriptor;
        let load = dual
            .load_scenario(&e2e_model::ScenarioInputs::default())
            .expect("rerouting_prack carries a load body");
        assert_eq!(load.id(), "rerouting_prack");
        assert!(dual.needs_bob2);
        // Functional-only shapes carry no load body.
        assert!(reg["basic-call"].descriptor.load.is_none());
    }

    /// The registry is OPEN: a third-party crate declares its descriptor in an
    /// extended `ShapeRegistry` and attaches its own functional body — served
    /// through the same unified path as the shipped shapes.
    #[test]
    fn third_party_shape_registers_descriptor_and_body() {
        struct VendorShape;
        #[async_trait(?Send)]
        impl crate::shape::CallflowShape for VendorShape {
            fn agents(&self) -> &[&str] {
                &["alice", "bob1"]
            }
            async fn run(&self, _rt: &mut InfraRuntime, _input: &Input) {
                unreachable!("registration-only test body")
            }
        }

        let mut shapes = ShapeRegistry::with_defaults();
        shapes.register(
            e2e_model::ShapeDescriptor::new("vendor-flow")
                .anchors(&[crate::shape::Anchor::InitialInvite]),
        );
        let mut bodies = default_bodies();
        bodies.push(("vendor-flow", Box::new(VendorShape)));
        let reg = attach(&shapes, bodies);

        assert!(reg.contains_key("vendor-flow"));
        assert_eq!(reg.len(), default_bodies().len() + 1);
        // The catalog view (what validate_case consumes) resolves it too.
        assert!(e2e_model::ShapeCatalog::spec(&reg, "vendor-flow").is_some());
    }

    /// Attaching a body whose id has NO descriptor is a registration error, not
    /// a silent extra id — the one-id-space guarantee.
    #[test]
    #[should_panic(expected = "no ShapeDescriptor registered for functional body")]
    fn body_without_descriptor_panics() {
        struct Orphan;
        #[async_trait(?Send)]
        impl crate::shape::CallflowShape for Orphan {
            async fn run(&self, _rt: &mut InfraRuntime, _input: &Input) {}
        }
        let _ = attach(&ShapeRegistry::with_defaults(), vec![(
            "orphan-shape",
            Box::new(Orphan) as Box<dyn CallflowShape>,
        )]);
    }
}
