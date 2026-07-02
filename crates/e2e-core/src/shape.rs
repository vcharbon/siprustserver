//! Callflow shapes (ADR-0018) + the canonical Message-anchor vocabulary
//! (ADR-0019). A shape is a compiled-Rust message-sequence template parameterised
//! over [`Input`], publishing the anchors it produces. The *same* shape runs over
//! any [`InfraShape`](crate::infra::InfraShape).
//!
//! The pure vocabulary ([`Anchor`], the [`Input`] core) moved to the
//! dependency-light `e2e-model` crate; the run trait stays here because it
//! drives an [`InfraRuntime`]. The bridge below folds `dyn CallflowShape` onto
//! [`e2e_model::ShapeSpec`] so the moved `validate_case` consumes the compiled
//! registry unchanged.

use async_trait::async_trait;

use crate::infra::InfraRuntime;

pub use e2e_model::shape::Anchor;
/// The shared input CORE a Test case supplies to a shape (moved to
/// `e2e-model` as `CoreInput`; the historical `shape::Input` name is kept).
pub use e2e_model::shape::CoreInput as Input;

/// Whether a Callflow shape exchanges RTP audio alongside the signaling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaMode {
    /// Signaling only.
    Off,
    /// Each Test Agent opens a `MediaEndpoint` on the same `SignalingNetwork`,
    /// streams its deterministic reference clip, and records inbound audio;
    /// per-agent `.wav` artifacts + the classifier verdict land on the result.
    Exchange,
}

/// A compiled Callflow shape — selected (not authored) from the registry.
#[async_trait(?Send)]
pub trait CallflowShape {
    /// Stable id used by Test cases / campaigns to select this shape.
    fn id(&self) -> &str;
    /// The anchors this shape publishes (per-agent at runtime).
    fn anchors(&self) -> &[Anchor];
    /// The Test-agent roster this shape drives — the names a check's
    /// `<agent>.<anchor>` selector may bind to (e.g. `["alice", "bob1"]`). The
    /// `<agent>.<anchor>` autocomplete surface (ADR-0019) is the cartesian
    /// product `agents() × anchors()`. Defaults empty for shapes that have not
    /// declared a roster yet (no selector completion, but never a wrong one).
    ///
    /// NOTE (experimental): this is an explicit list mirroring the names the
    /// shape passes to `rt.anchor(..)` in `run`; the no-drift design folds it
    /// into a per-shape `Agent` enum `run` is forced to use. Keep this in step
    /// with `run` until that lands.
    fn agents(&self) -> &[&str] {
        &[]
    }
    /// Input core/extra field names this shape *requires* (beyond the always-
    /// optional core overrides). A Test case missing one is incompatible at
    /// load time.
    fn required_input(&self) -> &[&str] {
        &[]
    }
    /// Whether this shape exchanges media (off by default).
    fn media(&self) -> MediaMode {
        MediaMode::Off
    }
    /// The JSON schema for this shape's authoring **parameters** — the typed
    /// fields that ride the Test case's `input.extras`. When `Some`, the per-shape
    /// Test-case schema splices it in place of the open `extras` map, so the
    /// editor suggests documented, typed fields instead of "guess what goes here".
    /// Pair it with a `#[derive(Deserialize, JsonSchema)]` params struct the shape
    /// reads via [`Input::params`](crate::model::Input::params). `None` (default)
    /// = no parameters; `extras` stays an open object.
    fn params_schema(&self) -> Option<serde_json::Value> {
        None
    }
    /// Drive the flow over the given Infra runtime. Receives the full authored
    /// [`Input`](crate::model::Input) — the shared `core` (From/To/R-URI) plus the
    /// shape's typed `extras` (read via [`Input::params`](crate::model::Input::params),
    /// schema'd by [`params_schema`](Self::params_schema)). Assertion failures
    /// panic in-line (the harness philosophy); the run-core isolates the panic per
    /// cell.
    async fn run(&self, rt: &mut InfraRuntime, input: &crate::model::Input);
}

/// The bridge onto the moved model: a compiled `dyn CallflowShape` *is* the
/// load-time metadata `e2e_model::validate_case` consumes, so the registry
/// (`BTreeMap<String, Box<dyn CallflowShape>>`) passes through unchanged.
impl<'a> e2e_model::ShapeSpec for dyn CallflowShape + 'a {
    fn anchors(&self) -> &[Anchor] {
        CallflowShape::anchors(self)
    }
    fn required_input(&self) -> &[&str] {
        CallflowShape::required_input(self)
    }
}
