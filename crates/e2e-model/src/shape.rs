//! The canonical Message-anchor vocabulary (ADR-0019), the shared input core
//! every Callflow shape receives, and [`ShapeSpec`] — the load-time metadata
//! slice of a shape the model validates against. The full run trait
//! (`CallflowShape`, which drives an `InfraRuntime`) lives in `e2e-core`; this
//! crate only ever needs to ask a shape *what it publishes and requires*.

/// The canonical, project-wide Message-anchor vocabulary. A Callflow shape
/// publishes the subset it produces; a Check binds to `<agent>.<anchor>`
/// (ADR-0019). Extend deliberately — adding a common anchor is a project-wide act.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Anchor {
    InitialInvite,
    ReInvite,
    FirstProvisional,
    Answer,
    Ack,
    Bye,
    Refer,
    Prack,
}

impl Anchor {
    /// The canonical surface name used in authored JSON (`bob1.initialInvite`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Anchor::InitialInvite => "initialInvite",
            Anchor::ReInvite => "reInvite",
            Anchor::FirstProvisional => "firstProvisional",
            Anchor::Answer => "answer",
            Anchor::Ack => "ack",
            Anchor::Bye => "bye",
            Anchor::Refer => "refer",
            Anchor::Prack => "prack",
        }
    }

    pub const ALL: &'static [Anchor] = &[
        Anchor::InitialInvite,
        Anchor::ReInvite,
        Anchor::FirstProvisional,
        Anchor::Answer,
        Anchor::Ack,
        Anchor::Bye,
        Anchor::Refer,
        Anchor::Prack,
    ];

    /// Parse a surface name back into the vocabulary (`None` = not a canonical
    /// anchor — a load-time validation error, never a silent pass).
    pub fn parse(name: &str) -> Option<Anchor> {
        Anchor::ALL.iter().copied().find(|a| a.as_str() == name)
    }
}

/// The shared input CORE a Test case supplies to a shape: From / To / R-URI
/// overrides (the numbers), each optional. This is both the runtime input and
/// the `core` of the authored JSON [`Input`](crate::model::Input) — headers /
/// timers join when the harness builder can honour them (no silent fields).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CoreInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ruri: Option<String>,
}

/// The **load-time metadata** half of a Callflow shape — everything
/// [`validate_case`](crate::model::validate_case) needs to judge a Test case's
/// compatibility, split from the heavy run trait (`CallflowShape` in
/// `e2e-core`, which bridges its trait objects onto this) so the axis model
/// stays dependency-light.
pub trait ShapeSpec {
    /// The anchors this shape publishes (per-agent at runtime).
    fn anchors(&self) -> &[Anchor];
    /// Input core/extra field names this shape *requires* (beyond the always-
    /// optional core overrides). A Test case missing one is incompatible at
    /// load time.
    fn required_input(&self) -> &[&str] {
        &[]
    }
}

/// A **catalog of shapes by id** — the lookup surface
/// [`validate_case`](crate::model::validate_case) consumes, so the one
/// validation serves every registry representation: the unified
/// [`ShapeRegistry`](crate::registry::ShapeRegistry) (descriptors), `e2e-core`'s
/// descriptor+functional-body map, and any plain test fixture map.
pub trait ShapeCatalog {
    /// The load-time metadata of shape `id`, if registered.
    fn spec(&self, id: &str) -> Option<&dyn ShapeSpec>;
    /// Every registered id (for precise unknown-id errors), sorted.
    fn ids(&self) -> Vec<String>;
}

/// Present a registry entry's load-time metadata view. Implemented for boxed
/// specs (test fixtures) and — downstream — for `e2e-core`'s
/// descriptor+functional-body entry, so ANY `BTreeMap<String, impl AsShapeSpec>`
/// is a [`ShapeCatalog`] despite the orphan rule.
pub trait AsShapeSpec {
    fn as_spec(&self) -> &dyn ShapeSpec;
}

impl<S: ShapeSpec> AsShapeSpec for Box<S> {
    fn as_spec(&self) -> &dyn ShapeSpec {
        self.as_ref()
    }
}

/// Any ordered map of spec-bearing entries is a catalog.
impl<E: AsShapeSpec> ShapeCatalog for std::collections::BTreeMap<String, E> {
    fn spec(&self, id: &str) -> Option<&dyn ShapeSpec> {
        self.get(id).map(AsShapeSpec::as_spec)
    }
    fn ids(&self) -> Vec<String> {
        self.keys().cloned().collect()
    }
}

impl ShapeCatalog for crate::registry::ShapeRegistry {
    fn spec(&self, id: &str) -> Option<&dyn ShapeSpec> {
        self.get(id).map(|d| d as &dyn ShapeSpec)
    }
    fn ids(&self) -> Vec<String> {
        crate::registry::ShapeRegistry::ids(self)
    }
}
